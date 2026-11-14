//! `chloectx stores` and `chloectx prune`: every store on this machine, the
//! project each records, and removing the ones nothing will use again.
//!
//! Each project directory a session ran in gets a store, including the
//! throwaway folders tools make under the system's temporary directory, so
//! stores pile up. A store whose project folder is gone, or sits in a
//! temporary folder and has not been used for a day, or names no project
//! and has not been used for a month, is a candidate for `prune`.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::ui::{list_stores, StoreInfo};

/// Why a store may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Its project folder no longer exists.
    Gone,
    /// Its project is in a temporary folder, and it was not used for a day.
    Temporary,
    /// It names no project, and was not used for a month.
    Unknown,
    /// Not used for longer than `--older-than`.
    Idle,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Gone => "project folder gone",
            Reason::Temporary => "temporary folder",
            Reason::Unknown => "project unknown",
            Reason::Idle => "not used lately",
        }
    }
}

/// One store, with its whole size on disk.
#[derive(Clone, Debug)]
pub struct Entry {
    pub info: StoreInfo,
    pub bytes: u64,
    pub running: bool,
}

/// Whether `p` is inside a temporary folder: the system's, `/tmp`, or
/// macOS's per-user `/var/folders`.
pub fn is_temporary(p: &str) -> bool {
    let tmp = std::env::temp_dir();
    let tmp_c = tmp.canonicalize().unwrap_or_else(|_| tmp.clone());
    let roots = [tmp.to_string_lossy().into_owned(), tmp_c.to_string_lossy().into_owned(), "/tmp/".into(), "/private/tmp/".into(), "/var/folders/".into(), "/private/var/folders/".into(), "/var/tmp/".into()];
    roots.iter().any(|r| {
        let r = r.trim_end_matches('/');
        !r.is_empty() && p.strip_prefix(r).is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The bytes of every file under `dir`, each file once (kept snapshots are
/// hard links to the store's segments).
pub fn dir_bytes(dir: &Path) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.path().symlink_metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if m.nlink() > 1 && !seen.insert((m.dev(), m.ino())) {
                        continue;
                    }
                }
                total += m.len();
            }
        }
    }
    total
}

/// Every store under `home`, most recently used first.
pub fn survey(home: &Path) -> Vec<Entry> {
    list_stores(home)
        .into_iter()
        .map(|info| {
            // A store from before the daemon wrote project.txt names its
            // project in daemon.log only: written down now.
            if let Some(p) = &info.project {
                let f = info.dir.join("project.txt");
                if !f.exists() {
                    let _ = std::fs::write(f, format!("{p}\n"));
                }
            }
            let running = std::os::unix::net::UnixStream::connect(crate::paths::StorePaths::new(&info.dir).socket).is_ok();
            Entry { bytes: dir_bytes(&info.dir), running, info }
        })
        .collect()
}

/// A store whose project folder is missing is not offered to `prune` for
/// this long after its last use: the folder may have been moved or renamed
/// (`chloectx move` attaches the store to its new place), or be on a drive
/// that is not mounted (CTX-11).
pub const MISSING_GRACE: i64 = 7 * 86_400;

/// Whether `p` is on a removable or network drive whose mount point is not
/// there now (`/Volumes/X`, `/media/user/X`, `/run/media/user/X`, `/mnt/X`):
/// its folder is not gone, only out of reach.
pub fn on_unmounted_drive(p: &str) -> bool {
    let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
    let depth = match parts.as_slice() {
        ["Volumes", ..] | ["mnt", ..] => 2,
        ["media", ..] => 3,
        ["run", "media", ..] => 4,
        _ => return false,
    };
    parts.len() > depth - 1 && !Path::new(&format!("/{}", parts[..depth.min(parts.len())].join("/"))).is_dir()
}

/// What is known of a store's project folder, for a person.
pub fn project_state(e: &Entry, now: i64) -> Option<&'static str> {
    let p = e.info.project.as_deref()?;
    if Path::new(p).is_dir() {
        None
    } else if on_unmounted_drive(p) {
        Some("drive not mounted")
    } else if now - e.info.modified < MISSING_GRACE {
        Some("project folder missing: moved?")
    } else {
        None
    }
}

/// Why `e` may be pruned, if it may: `older_than` (seconds) adds every
/// store not used for that long. A running store never is, nor one whose
/// folder is on a drive that is not mounted, or went missing in the last
/// week (it may have moved).
pub fn reason(e: &Entry, now: i64, older_than: Option<i64>) -> Option<Reason> {
    if e.running {
        return None;
    }
    let idle = now - e.info.modified;
    match e.info.project.as_deref() {
        Some(p) if !Path::new(p).is_dir() && (on_unmounted_drive(p) || idle < MISSING_GRACE) => return None,
        Some(p) if !Path::new(p).is_dir() => return Some(Reason::Gone),
        Some(p) if is_temporary(p) && idle >= 86_400 => return Some(Reason::Temporary),
        None if idle >= 30 * 86_400 => return Some(Reason::Unknown),
        _ => {}
    }
    older_than.filter(|o| idle >= *o).map(|_| Reason::Idle)
}

/// Bytes for a person: 12 KB, 3.4 MB, 1.2 GB.
pub fn human(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.0} KB", (b / 1e3).ceil())
    }
}

fn line(e: &Entry, now: i64, why: Option<Reason>) -> String {
    let project = e.info.project.clone().unwrap_or_else(|| format!("(no project recorded; store {})", e.info.id));
    let mut s = format!("{:>9}  {:>9}  {project}", crate::recall::fmt_ago(e.info.modified, now), human(e.bytes));
    if e.running {
        s += "  [running]";
    }
    if let Some(r) = why {
        s += &format!("  [{}]", r.as_str());
    } else if let Some(st) = project_state(e, now) {
        s += &format!("  [{st}]");
    }
    s
}

/// The hint under a listing that shows a store whose folder is missing or
/// gone.
const MOVED_HINT: &str = "If a project folder was moved or renamed, `chloectx move OLD NEW` attaches its store to the new folder.";

/// `chloectx stores`: one line per store, and what `prune` would remove.
pub fn list(home: &Path, as_json: bool) -> i32 {
    let now = crate::now();
    let all = survey(home);
    if as_json {
        let v: Vec<Json> = all
            .iter()
            .map(|e| {
                let mut j = e.info.json();
                j["bytes"] = json!(e.bytes);
                j["running"] = json!(e.running);
                j["prunable"] = json!(reason(e, now, None).map(Reason::as_str));
                j
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!({"home": home, "stores": v})).unwrap());
        return 0;
    }
    if all.is_empty() {
        println!("No stores in {} yet: one is made for each project the first time a Claude Code session runs there.", home.display());
        return 0;
    }
    println!("{:>9}  {:>9}  project", "last used", "size");
    for e in &all {
        println!("{}", line(e, now, reason(e, now, None)));
    }
    let total: u64 = all.iter().map(|e| e.bytes).sum();
    let gone: Vec<&Entry> = all.iter().filter(|e| reason(e, now, None).is_some()).collect();
    let missing = all.iter().any(|e| e.info.project.as_deref().is_some_and(|p| !Path::new(p).is_dir()));
    println!("\n{} store{}, {} in {}.", all.len(), if all.len() == 1 { "" } else { "s" }, human(total), home.display());
    if !gone.is_empty() {
        let (n, b) = (gone.len(), human(gone.iter().map(|e| e.bytes).sum()));
        match n {
            1 => println!("1 of them ({b}) belongs to a folder that is gone, temporary or unknown: `chloectx prune` removes it."),
            _ => println!("{n} of them ({b}) belong to folders that are gone, temporary or unknown: `chloectx prune` removes them."),
        }
    }
    if missing {
        println!("{MOVED_HINT}");
    }
    0
}

pub struct PruneOptions {
    /// Also stores not used for this many days.
    pub older_than_days: Option<f64>,
    /// Remove without asking.
    pub yes: bool,
    /// Only list what would go.
    pub dry_run: bool,
}

/// Stops a store's daemon (if one answers) and removes its directory.
fn remove(e: &Entry) -> std::io::Result<()> {
    let sock = crate::paths::StorePaths::new(&e.info.dir).socket;
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        let _ = crate::daemon::request(&e.info.dir, e.info.project.as_deref().unwrap_or(""), &json!({"op": "shutdown"}), Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::remove_dir_all(&e.info.dir)?;
    if !sock.starts_with(&e.info.dir) {
        let _ = std::fs::remove_file(sock);
    }
    Ok(())
}

/// `chloectx prune`: lists the stores that may go and removes them, after
/// asking (or with `--yes`).
pub fn prune(home: &Path, o: &PruneOptions) -> i32 {
    let now = crate::now();
    let older = o.older_than_days.map(|d| (d * 86_400.0) as i64);
    let all = survey(home);
    let picked: Vec<(&Entry, Reason)> = all.iter().filter_map(|e| reason(e, now, older).map(|r| (e, r))).collect();
    if picked.is_empty() {
        let kept: Vec<&Entry> = all.iter().filter(|e| project_state(e, now).is_some()).collect();
        if kept.is_empty() {
            println!("Nothing to prune: all {} store{} in {} belong to project folders that exist.", all.len(), if all.len() == 1 { "" } else { "s" }, home.display());
        } else {
            println!("Nothing to prune now. Kept although their folder is missing (moved, or on a drive that is not mounted):");
            for e in kept {
                println!("{}", line(e, now, None));
            }
            println!("{MOVED_HINT}");
        }
        return 0;
    }
    for (e, r) in &picked {
        println!("{}", line(e, now, Some(*r)));
    }
    if picked.iter().any(|(_, r)| *r == Reason::Gone) {
        println!("\n{MOVED_HINT}");
    }
    let bytes: u64 = picked.iter().map(|(e, _)| e.bytes).sum();
    let what = format!("{} store{} ({})", picked.len(), if picked.len() == 1 { "" } else { "s" }, human(bytes));
    if o.dry_run {
        println!("\nWould remove {what}. Nothing was changed.");
        return 0;
    }
    if !o.yes {
        use std::io::{BufRead, IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            println!("\nWould remove {what}. Run `chloectx prune --yes` to remove them (their memories cannot be brought back).");
            return 0;
        }
        print!("\nRemove {what}? Their memories cannot be brought back. [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing was removed.");
            return 0;
        }
    }
    let mut failed = 0;
    for (e, _) in &picked {
        if let Err(err) = remove(e) {
            failed += 1;
            eprintln!("chloectx prune: {}: {err}", e.info.dir.display());
        }
    }
    println!("Removed {} of {what}.", picked.len() - failed);
    i32::from(failed > 0)
}

/// `chloectx move OLD NEW`: the store of the project that was at `old`
/// becomes the store of `new` (a project moved or renamed, CTX-11). Its
/// daemon is stopped first; `new` must exist and have no store of its own.
pub fn relink(home: &Path, old: &Path, new: &Path) -> std::result::Result<String, String> {
    let new = new.canonicalize().map_err(|e| format!("{}: {e}", new.display()))?;
    if !new.is_dir() {
        return Err(format!("{} is not a folder", new.display()));
    }
    let old_s = old.to_string_lossy().trim_end_matches('/').to_string();
    let old_c = old.canonicalize().ok().map(|p| p.to_string_lossy().into_owned());
    let all = list_stores(home);
    let from = all
        .iter()
        .find(|s| s.project.as_deref().is_some_and(|p| p == old_s || Some(p) == old_c.as_deref()))
        .or_else(|| all.iter().find(|s| s.dir.file_name().is_some_and(|n| n.to_string_lossy() == crate::paths::project_slug(old))))
        .ok_or_else(|| format!("no store records the project {} (`chloectx stores` lists them)", old.display()))?;
    let to = home.join("projects").join(crate::paths::project_slug(&new));
    if to == from.dir {
        return Err(format!("the store of {} is already that of {}", old.display(), new.display()));
    }
    if to.join("memory.strata").exists() {
        return Err(format!(
            "{} already has a store of its own ({}); a session ran there since the move. Remove it first (it is not merged), then run this again",
            new.display(),
            to.display()
        ));
    }
    for d in [&from.dir, &to] {
        let sock = crate::paths::StorePaths::new(d).socket;
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            let _ = crate::daemon::request_running(d, &json!({"op": "shutdown"}), Duration::from_secs(10));
            let until = std::time::Instant::now() + Duration::from_secs(5);
            while std::os::unix::net::UnixStream::connect(&sock).is_ok() && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    if to.exists() {
        // Only what a client left there without a store (a lock, a log).
        std::fs::remove_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    let old_sock = crate::paths::StorePaths::new(&from.dir).socket;
    std::fs::rename(&from.dir, &to).map_err(|e| format!("{} to {}: {e}", from.dir.display(), to.display()))?;
    if !old_sock.starts_with(&from.dir) {
        let _ = std::fs::remove_file(old_sock);
    }
    let _ = std::fs::remove_file(to.join("engram.sock"));
    std::fs::write(to.join("project.txt"), format!("{}\n", new.display())).map_err(|e| e.to_string())?;
    Ok(format!(
        "The memory of {} is now that of {} ({}). Records keep the paths they were written with.",
        from.project.as_deref().unwrap_or(&old_s),
        new.display(),
        to.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(home: &Path, id: &str, project: Option<&str>, age_days: i64) {
        let d = home.join("projects").join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("memory.strata"), vec![0u8; 5000]).unwrap();
        if let Some(p) = project {
            std::fs::write(d.join("project.txt"), format!("{p}\n")).unwrap();
        }
        let t = std::time::SystemTime::now() - Duration::from_secs((age_days * 86_400) as u64);
        for f in ["memory.strata"] {
            std::fs::File::options().write(true).open(d.join(f)).unwrap().set_modified(t).unwrap();
        }
    }

    #[test]
    fn picks_the_stores_nothing_will_use_again() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        // A project outside the temporary folders: this crate's own.
        let live = Path::new(env!("CARGO_MANIFEST_DIR"));
        let tmp_project = std::env::temp_dir().join(format!("chloectx-prune-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_project).unwrap();
        store(&home, "app-1", Some(live.to_str().unwrap()), 90);
        store(&home, "gone-2", Some("/nowhere/at/all"), 10);
        store(&home, "tmp-3", Some(tmp_project.to_str().unwrap()), 2);
        store(&home, "tmpnew-4", Some(tmp_project.to_str().unwrap()), 0);
        store(&home, "unknown-5", None, 40);
        store(&home, "unknownnew-6", None, 1);
        store(&home, "moved-7", Some("/nowhere/moved"), 2);
        store(&home, "moved-old-8", Some("/nowhere/moved/long/ago"), 30);
        store(&home, "drive-9", Some("/Volumes/NoSuchDrive-chloectx/work/app"), 60);
        let now = crate::now();
        let all = survey(&home);
        let why = |id: &str, older: Option<i64>| reason(all.iter().find(|e| e.info.id == id).unwrap(), now, older);
        assert_eq!(why("app-1", None), None, "a project that exists stays, however old");
        assert_eq!(why("gone-2", None), Some(Reason::Gone));
        assert_eq!(why("tmp-3", None), Some(Reason::Temporary));
        assert_eq!(why("tmpnew-4", None), None, "a temporary folder in use today");
        assert_eq!(why("unknown-5", None), Some(Reason::Unknown));
        assert_eq!(why("unknownnew-6", None), None);
        assert_eq!(why("app-1", Some(30 * 86_400)), Some(Reason::Idle));
        assert_eq!(why("moved-7", None), None, "a folder missing for a few days may have moved");
        assert_eq!(why("moved-old-8", None), Some(Reason::Gone));
        assert_eq!(why("drive-9", None), None, "a drive that is not mounted");

        // A dry run changes nothing; --yes removes the four.
        assert_eq!(prune(&home, &PruneOptions { older_than_days: None, yes: false, dry_run: true }), 0);
        assert_eq!(survey(&home).len(), 9);
        assert_eq!(prune(&home, &PruneOptions { older_than_days: None, yes: true, dry_run: false }), 0);
        let left: Vec<String> = survey(&home).into_iter().map(|e| e.info.id).collect();
        assert_eq!(left.len(), 5, "{left:?}");
        assert!(left.contains(&"app-1".to_string()) && left.contains(&"tmpnew-4".to_string()) && left.contains(&"unknownnew-6".to_string()));
        let _ = std::fs::remove_dir_all(&tmp_project);
    }

    /// CTX-11: a moved project gets its store back with `chloectx move`.
    #[test]
    fn a_moved_project_takes_its_store_along() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let old = t.path().join("old-name");
        let new = t.path().join("new-name");
        std::fs::create_dir_all(&new).unwrap();
        let old_s = old.to_str().unwrap().to_string();
        let slug = crate::paths::project_slug(&old);
        store(&home, &slug, Some(&old_s), 0);
        std::fs::write(home.join("projects").join(&slug).join("events.log"), "{}\n").unwrap();
        let msg = relink(&home, &old, &new).unwrap();
        assert!(msg.contains("new-name"), "{msg}");
        let to = home.join("projects").join(crate::paths::project_slug(&new));
        assert!(to.join("memory.strata").is_file() && to.join("events.log").is_file());
        assert_eq!(std::fs::read_to_string(to.join("project.txt")).unwrap().trim(), new.canonicalize().unwrap().to_str().unwrap());
        assert!(!home.join("projects").join(&slug).exists());
        // Not again, and not over a store the new folder has.
        assert!(relink(&home, &old, &new).is_err());
        let other = t.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        store(&home, &crate::paths::project_slug(&other), Some(other.to_str().unwrap()), 0);
        assert!(relink(&home, &new, &other).unwrap_err().contains("already has a store"));
    }

    #[test]
    fn unmounted_drives() {
        assert!(on_unmounted_drive("/Volumes/NoSuchDrive-chloectx/code/app"));
        assert!(on_unmounted_drive("/media/me/NoSuchDrive-chloectx/app"));
        assert!(!on_unmounted_drive("/home/me/app"));
        assert!(!on_unmounted_drive("/Volumes"));
    }

    #[test]
    fn temporary_folders() {
        assert!(is_temporary("/tmp/x"));
        assert!(is_temporary("/private/var/folders/ab/T/project-12"));
        assert!(!is_temporary("/tmpfoo/x"));
        assert!(!is_temporary("/Users/me/code/app"));
        assert_eq!(human(1_234_000_000), "1.2 GB");
        assert_eq!(human(5_000), "5 KB");
    }
}
