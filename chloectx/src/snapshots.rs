//! Kept snapshots: copies of the store, by name, that a receipt's recall
//! can be replayed on ([`crate::receipt`]).
//!
//! Each is a checkpoint of the store in ChloeDB's directory layout
//! (`Database::checkpoint`), under `snapshots/<name>/` in the store's
//! directory. A checkpoint hard-links the segment files of the one before
//! it that no merge has replaced since, so the bytes a snapshot adds are
//! the segments written or merged since the one before; the first is a
//! full copy (the live store is one file, which a directory checkpoint
//! cannot share bytes with). Taking one is not cheap: the checkpoint
//! flushes the store's memtable, then reads and checks every segment, so
//! automatic snapshots are kept at most once an hour by default
//! ([`Retention::min_interval_secs`]). Nothing in the store's own file
//! changes: a kept snapshot is a backup that is itself a database, opened
//! read-only to replay.
//!
//! `snapshots/index.json` lists them: the name, the commit it holds, the
//! commit the store was at when it was taken (the two differ by the flush
//! the checkpoint makes, which changes no row), the time, and whether it
//! was kept automatically (then pruned by age and count) or by name (kept
//! until dropped). Anything else under `snapshots/` (a checkpoint a stopped
//! daemon left half written, a directory a crash left after its entry was
//! removed) belongs to no snapshot and is removed by [`Snapshots::sweep`],
//! which the daemon runs when it starts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use chloedb::db::Db;
use chloedb::storage::{CheckpointOptions, Layout};

use crate::embed::Embedder;
use crate::store::Store;
use crate::{err, now, Result};

/// One kept snapshot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Kept {
    pub name: String,
    /// The commit the copy holds.
    pub txn: u64,
    /// The commit readers of the store saw just before the copy was made:
    /// a receipt read at this commit replays exactly on this snapshot.
    pub at: u64,
    pub ts: i64,
    /// Kept automatically after a recall (pruned), or by name (kept).
    pub auto: bool,
    pub records: u64,
    /// Bytes the snapshot wrote (segments linked from the one before it
    /// are not counted): about the store's size when it linked nothing.
    #[serde(default)]
    pub bytes_written: u64,
    /// It had one before it to link from but linked none of its segments
    /// (a file system without hard links copies the whole store each time).
    #[serde(default)]
    pub copied: bool,
}

/// How long automatic snapshots are kept.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Retention {
    /// Keep a snapshot after each recall that showed records, when the
    /// store changed since the last one.
    pub after_recall: bool,
    /// At most one automatic snapshot this often (0: after every recall
    /// that showed records, which flushes the store each turn and makes
    /// its merges rewrite each record several times more). Recalls in
    /// between replay on the nearest later one, which says how far it is.
    pub min_interval_secs: i64,
    /// Automatic snapshots older than this are dropped.
    pub keep_days: i64,
    /// At most this many automatic snapshots (the oldest go first).
    pub max_auto: usize,
    /// Automatic snapshots older than this many days are thinned to one a
    /// day (the last of each day, UTC); 0 keeps them all.
    pub daily_after_days: i64,
    /// The most bytes the snapshots take, as a multiple of the store's
    /// file (each file counted once, however many snapshots link it): past
    /// it, the oldest automatic snapshots go, though never the newest
    /// one. 0 sets no budget. Hourly snapshots of a store in use took
    /// about 3 times the store after a week with no budget.
    pub budget_store_ratio: f64,
    /// A budget in bytes, in place of the ratio (0: the ratio's, and at
    /// least [`MIN_BUDGET`]).
    pub max_bytes: u64,
}

/// The least a budget from [`Retention::budget_store_ratio`] allows, so
/// that a small store keeps a few snapshots.
pub const MIN_BUDGET: u64 = 32 << 20;

impl Default for Retention {
    fn default() -> Self {
        Retention { after_recall: true, min_interval_secs: 3600, keep_days: 30, max_auto: 2000, daily_after_days: 3, budget_store_ratio: 1.0, max_bytes: 0 }
    }
}

impl Retention {
    /// The snapshots' budget in bytes for a store of `store_bytes`, or
    /// `None` for no budget.
    pub fn budget(&self, store_bytes: u64) -> Option<u64> {
        if self.max_bytes > 0 {
            return Some(self.max_bytes);
        }
        (self.budget_store_ratio > 0.0).then(|| ((store_bytes as f64 * self.budget_store_ratio) as u64).max(MIN_BUDGET))
    }
}

/// The kept snapshots of one store.
pub struct Snapshots {
    dir: PathBuf,
    lock: Mutex<()>,
    /// Forgets whose segments are not rewritten yet ([`Snapshots::hold`]).
    holds: std::sync::atomic::AtomicUsize,
}

/// A name a person gives a snapshot: letters, digits, `.`, `_` and `-`,
/// not starting with `.`, at most 64 bytes.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Snapshots {
    /// The snapshots of the store in `store_dir`.
    pub fn new(store_dir: &Path) -> Snapshots {
        Snapshots { dir: store_dir.join("snapshots"), lock: Mutex::new(()), holds: Default::default() }
    }

    /// No snapshot is taken until [`Snapshots::release`]: a forget holds
    /// them from its delete until the segments that held the record are
    /// rewritten, which happens after its answer, so that none copies the
    /// record's bytes meanwhile.
    pub fn hold(&self) {
        self.holds.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    pub fn release(&self) {
        let _ = self.holds.fetch_update(std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire, |n| n.checked_sub(1));
    }

    /// Whether a forget holds snapshots off now.
    pub fn held(&self) -> bool {
        self.holds.load(std::sync::atomic::Ordering::Acquire) > 0
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    /// Every kept snapshot, oldest first.
    pub fn list(&self) -> Vec<Kept> {
        let mut v: Vec<Kept> = std::fs::read_to_string(self.index_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        v.retain(|k| self.dir.join(&k.name).is_dir());
        v.sort_by_key(|k| (k.at, k.ts));
        v
    }

    fn write_index(&self, v: &[Kept]) -> Result<()> {
        let tmp = self.dir.join("index.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v).map_err(|e| err(e.to_string()))?)?;
        std::fs::rename(&tmp, self.index_path())?;
        Ok(())
    }

    /// The snapshot a recall read at commit `txn` replays on exactly, if
    /// one was kept.
    pub fn exact(&self, txn: u64) -> Option<Kept> {
        self.list().into_iter().find(|k| k.at == txn || k.txn == txn)
    }

    /// The earliest snapshot kept at or after commit `txn`.
    pub fn after(&self, txn: u64) -> Option<Kept> {
        self.list().into_iter().find(|k| k.at >= txn)
    }

    pub fn get(&self, name: &str) -> Option<Kept> {
        self.list().into_iter().find(|k| k.name == name)
    }

    /// Whether an automatic snapshot of `db` would be taken now: the newest
    /// kept one does not hold its commit, and the last automatic one is at
    /// least `min_interval_secs` old.
    pub fn due(&self, db: &Db, r: &Retention) -> bool {
        let at = db.snapshot().txn();
        let all = self.list();
        if all.iter().rev().any(|k| k.at == at || k.txn == at) {
            return false;
        }
        r.min_interval_secs <= 0 || all.iter().rev().find(|k| k.auto).is_none_or(|k| now() - k.ts >= r.min_interval_secs)
    }

    /// Keeps a snapshot of `db` now: under `name`, or (automatic) named by
    /// its commit. An automatic one is not taken when the newest kept
    /// snapshot already holds the store's commit, nor within
    /// `min_interval` of the last automatic one.
    pub fn keep(&self, db: &Db, name: Option<&str>, r: &Retention) -> Result<Kept> {
        let _g = self.lock.lock().unwrap();
        if self.held() {
            return Err(err("a forget is rewriting the store's segments: no snapshot until it is done; try again in a moment"));
        }
        crate::paths::create_private_dir(&self.dir)?;
        let mut all = self.list();
        let auto = name.is_none();
        if auto && !self.due(db, r) {
            let at = db.snapshot().txn();
            if let Some(k) = all.iter().rev().find(|k| k.at == at || k.txn == at).or_else(|| all.iter().rev().find(|k| k.auto)) {
                return Ok(k.clone());
            }
        }
        let at = db.snapshot().txn();
        let name = match name {
            Some(n) if !valid_name(n) => return Err(err(format!("{n:?} is not a snapshot name: use letters, digits, '.', '_' and '-' (at most 64)"))),
            Some(n) if all.iter().any(|k| k.name == n) => return Err(err(format!("a snapshot named {n} is kept already"))),
            Some(n) => n.to_string(),
            None => format!("auto-{at}"),
        };
        let dest = self.dir.join(&name);
        if dest.exists() {
            // Left by a checkpoint the index never named (a crash between
            // the two): it is replaced.
            std::fs::remove_dir_all(&dest)?;
        }
        let link_from = all.iter().rev().map(|k| self.dir.join(&k.name)).find(|p| p.is_dir());
        let stats = match db.storage().checkpoint(&dest, &CheckpointOptions { layout: Layout::Directory, link_from: link_from.clone() }) {
            // The snapshot it would link from does not read (a removal a
            // crash cut short): a full copy, rather than no snapshot again.
            Err(_) if link_from.is_some() && !dest.exists() => db.storage().checkpoint(&dest, &CheckpointOptions { layout: Layout::Directory, link_from: None })?,
            r => r?,
        };
        // The commit readers saw when the checkpoint took the writer lock: a
        // write that committed after `at` was read is in the copy, so a
        // receipt read at `at` would not replay on it exactly.
        let at = if stats.source_txn > 0 { stats.source_txn } else { at };
        let copied = link_from.is_some() && stats.segments > 0 && stats.segments_linked == 0;
        let k = Kept { name, txn: stats.txn, at, ts: now(), auto, records: stats.live_docs, bytes_written: stats.bytes_written, copied };
        all.push(k.clone());
        self.write_index(&all)?;
        Ok(k)
    }

    /// Drops the automatic snapshots past their retention: older than
    /// `keep_days`, past `max_auto`, all but the last of each day once
    /// older than `daily_after_days`, then the oldest while the snapshots
    /// take more than the budget for a store of `store_bytes`
    /// ([`Retention::budget`]), keeping the newest. Named snapshots are
    /// kept, and count toward the budget.
    pub fn prune(&self, r: &Retention, store_bytes: u64) -> Result<usize> {
        let _g = self.lock.lock().unwrap();
        let all = self.list();
        let autos: Vec<&Kept> = all.iter().filter(|k| k.auto).collect();
        let over = autos.len().saturating_sub(r.max_auto);
        let t = now();
        let old = t - r.keep_days.max(0) * 86_400;
        let mut gone: Vec<String> = autos.iter().enumerate().filter(|(i, k)| *i < over || k.ts < old).map(|(_, k)| k.name.clone()).collect();
        // One a day past `daily_after_days`: the last of each day (they are
        // listed oldest first, so a later one of the same day replaces it).
        if r.daily_after_days > 0 {
            let thin = t - r.daily_after_days * 86_400;
            let mut last_of_day: std::collections::HashMap<i64, &str> = Default::default();
            let mut thinned = Vec::new();
            for k in autos.iter().filter(|k| k.ts < thin && !gone.contains(&k.name)) {
                if let Some(prev) = last_of_day.insert(k.ts.div_euclid(86_400), &k.name) {
                    thinned.push(prev.to_string());
                }
            }
            gone.extend(thinned);
        }
        let mut n = self.remove(all, &gone)?;
        if let Some(budget) = r.budget(store_bytes) {
            loop {
                let all = self.list();
                let autos: Vec<String> = all.iter().filter(|k| k.auto).map(|k| k.name.clone()).collect();
                if autos.len() <= 1 || self.bytes() <= budget {
                    break;
                }
                n += self.remove(all, &autos[..1])?;
            }
        }
        Ok(n)
    }

    /// Removes snapshots: the index first, then their directories, so that
    /// a crash between leaves a directory no entry names (which
    /// [`Snapshots::sweep`] removes) rather than an entry whose directory
    /// is half gone. Called with the lock held.
    fn remove(&self, all: Vec<Kept>, gone: &[String]) -> Result<usize> {
        if gone.is_empty() {
            return Ok(0);
        }
        let left: Vec<Kept> = all.into_iter().filter(|k| !gone.contains(&k.name)).collect();
        self.write_index(&left)?;
        for n in gone {
            let _ = std::fs::remove_dir_all(self.dir.join(n));
        }
        Ok(gone.len())
    }

    /// Drops a snapshot by name.
    pub fn drop_named(&self, name: &str) -> Result<bool> {
        let _g = self.lock.lock().unwrap();
        let all = self.list();
        if !all.iter().any(|k| k.name == name) {
            return Ok(false);
        }
        self.remove(all, &[name.to_string()])?;
        Ok(true)
    }

    /// Drops every automatic snapshot, and everything under `snapshots/`
    /// that no snapshot owns, after running `first` with no snapshot being
    /// taken meanwhile: a forget compacts the store there, so that no
    /// snapshot taken from then on holds the forgotten bytes, nor one taken
    /// while it ran. Returns what `first` returned, the automatic
    /// snapshots dropped, and the named ones left (which hold the old
    /// text until dropped by name).
    pub fn drop_automatic_after<T>(&self, first: impl FnOnce() -> T) -> Result<(T, usize, Vec<String>)> {
        let _g = self.lock.lock().unwrap();
        let t = first();
        let all = self.list();
        let gone: Vec<String> = all.iter().filter(|k| k.auto).map(|k| k.name.clone()).collect();
        let named: Vec<String> = all.iter().filter(|k| !k.auto).map(|k| k.name.clone()).collect();
        let n = self.remove(all, &gone)?;
        self.sweep_locked();
        Ok((t, n, named))
    }

    /// Removes what is under `snapshots/` and belongs to no kept snapshot:
    /// a checkpoint a stopped daemon left half written (its temporary
    /// name is random, so nothing else would ever find it), the directory
    /// of an entry removed by a crash midway. Their hard links pin segment
    /// files after the snapshots that shared them are pruned. Returns the
    /// number of entries removed.
    pub fn sweep(&self) -> usize {
        let _g = self.lock.lock().unwrap();
        self.sweep_locked()
    }

    fn sweep_locked(&self) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return 0 };
        let named: std::collections::HashSet<String> = std::fs::read_to_string(self.index_path())
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Kept>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|k| k.name)
            .collect();
        let mut n = 0;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == "index.json" || named.contains(&name) {
                continue;
            }
            let p = e.path();
            let r = if p.is_dir() && !p.is_symlink() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            if r.is_ok() {
                n += 1;
            }
        }
        n
    }

    /// The bytes the snapshots take on disk: each file counted once,
    /// however many snapshots hard-link it.
    pub fn bytes(&self) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fn walk(p: &Path, seen: &mut std::collections::HashSet<(u64, u64)>, total: &mut u64) {
            let Ok(rd) = std::fs::read_dir(p) else { return };
            for e in rd.flatten() {
                let Ok(m) = e.metadata() else { continue };
                if m.is_dir() {
                    walk(&e.path(), seen, total);
                } else if seen.insert((m.dev(), m.ino())) {
                    *total += m.len();
                }
            }
        }
        let (mut seen, mut total) = (Default::default(), 0);
        walk(&self.dir, &mut seen, &mut total);
        total
    }

    /// Opens a kept snapshot to read.
    pub fn open(&self, k: &Kept, embedder: Arc<dyn Embedder>) -> Result<Store> {
        Store::open_read_only(&self.dir.join(&k.name), embedder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot `name` taken `age` seconds ago holding `bytes` bytes.
    fn fake(s: &Snapshots, name: &str, age: i64, bytes: usize, auto: bool) -> Kept {
        let d = s.dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("seg"), vec![7u8; bytes]).unwrap();
        Kept { name: name.into(), txn: 1, at: 1, ts: now() - age, auto, records: 1, bytes_written: bytes as u64, copied: false }
    }

    #[test]
    fn old_snapshots_thin_to_one_a_day_and_all_keep_to_a_budget() {
        // X-F3: hourly snapshots took about 3 times the store after a
        // week, with no cap in bytes.
        let dir = tempfile::tempdir().unwrap();
        let s = Snapshots::new(dir.path());
        std::fs::create_dir_all(&s.dir).unwrap();
        let day = 86_400;
        let mut all = Vec::new();
        // Five days of hourly snapshots, ten an (8-hour) day, 1 KB each.
        for d in 0..5 {
            for h in 0..10 {
                // From the newest day back; within a day, an hour apart.
                let age = d * day + (10 - h) * 3600 + 60;
                all.push(fake(&s, &format!("auto-{d}-{h}"), age, 1024, true));
            }
        }
        all.push(fake(&s, "release", 6 * day, 1024, false));
        all.sort_by_key(|k| k.ts);
        s.write_index(&all).unwrap();
        let r = Retention { budget_store_ratio: 0.0, ..Retention::default() };
        s.prune(&r, 0).unwrap();
        let left = s.list();
        // Days 3 and 4 (older than 3 days) keep one each; days 0..=2 all ten;
        // the named one stays.
        let autos: Vec<&Kept> = left.iter().filter(|k| k.auto).collect();
        let by_day = |d: i64| autos.iter().filter(|k| k.name.starts_with(&format!("auto-{d}-"))).count();
        let days: Vec<usize> = (0..5).map(by_day).collect();
        assert_eq!(&days[..3], &[10, 10, 10], "{days:?}");
        // The older two span 33 hours: two or three UTC days, one each.
        assert!((2..=3).contains(&(days[3] + days[4])), "{days:?}");
        assert!(left.iter().any(|k| k.name == "release"));
        // A budget of 8 KB: the oldest automatic ones go, never the newest,
        // and the named one is kept (and counted).
        let r = Retention { max_bytes: 8 * 1024, ..Retention::default() };
        s.prune(&r, 0).unwrap();
        assert!(s.bytes() <= 8 * 1024 + 512, "{}", s.bytes());
        let left = s.list();
        assert!(left.iter().any(|k| k.name == "release") && left.iter().any(|k| k.name == "auto-0-9"), "{:?}", left.iter().map(|k| &k.name).collect::<Vec<_>>());
        // Past the budget with one automatic snapshot left: it stays.
        let r = Retention { max_bytes: 1, ..Retention::default() };
        s.prune(&r, 0).unwrap();
        assert_eq!(s.list().iter().filter(|k| k.auto).count(), 1);
    }

    #[test]
    fn the_budget_follows_the_store() {
        let r = Retention::default();
        assert_eq!(r.budget(1 << 30), Some(1 << 30));
        assert_eq!(r.budget(1 << 10), Some(MIN_BUDGET));
        assert_eq!(Retention { budget_store_ratio: 0.0, ..r.clone() }.budget(1 << 30), None);
        assert_eq!(Retention { max_bytes: 5, ..r }.budget(1 << 30), Some(5));
    }
}
