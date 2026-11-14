//! Where a project's store, socket and logs live.

use std::path::{Path, PathBuf};

/// Where the stores and the models live: `$CHLOECTX_HOME` (which the
/// programs set from `$ENGRAM_HOME`, its name before the rename), else
/// `chloectx` in the data folder (`$XDG_DATA_HOME`, else `~/.local/share`).
///
/// ChloeCtx was called Engram, and kept its stores in `<data>/engram`. A
/// machine that has that folder and no `<data>/chloectx` keeps using it, so
/// an upgrade moves nothing; [`OLD_HOME`] names it.
pub fn home() -> PathBuf {
    if let Some(d) = std::env::var_os("CHLOECTX_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    home_in(&data_dir())
}

/// The folder [`home`] picks in a data folder.
pub fn home_in(data: &Path) -> PathBuf {
    let new = data.join(HOME);
    let old = data.join(OLD_HOME);
    if !new.exists() && old.is_dir() {
        old
    } else {
        new
    }
}

/// The folder's name in the data folder.
pub const HOME: &str = "chloectx";
/// Its name before the rename, which a machine that has it keeps.
pub const OLD_HOME: &str = "engram";

/// `$XDG_DATA_HOME`, else `~/.local/share`.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share")
}

/// Claude Code's user settings: `$CLAUDE_CONFIG_DIR/settings.json`, else
/// `~/.claude/settings.json`.
pub fn user_settings() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("settings.json");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claude/settings.json")
}

/// Claude Code's own state, which holds the user-scope MCP servers:
/// `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`. ChloeCtx reads
/// it and changes it only through `claude mcp`.
pub fn claude_json() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join(".claude.json");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claude.json")
}

/// A short, stable directory name for a project: its last path component
/// plus a hash of the full path (Unix socket paths are limited to 107 bytes).
pub fn project_slug(project: &Path) -> String {
    let abs = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
    let s = abs.to_string_lossy();
    let base: String = abs
        .file_name()
        .map(|n| n.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(32).collect())
        .unwrap_or_else(|| "root".into());
    format!("{base}-{:08x}", crate::embed::fnv(&s) as u32)
}

/// The store directory of a project: one per repository, so that sessions
/// in its linked worktrees share the memory of its main checkout (see
/// [`store_root`]).
pub fn store_dir(project: &Path) -> PathBuf {
    home().join("projects").join(project_slug(&store_root(project)))
}

/// `CHLOECTX_PER_WORKTREE=1`: each git worktree keeps a store of its own,
/// as before stores were shared by a repository's worktrees.
pub fn per_worktree() -> bool {
    std::env::var("CHLOECTX_PER_WORKTREE").is_ok_and(|v| v == "1")
}

/// The folder a project's store is keyed by: for a folder in a linked git
/// worktree, the same folder in the repository's main working tree (for a
/// bare repository, which has none, its git directory); for any other
/// folder (a main checkout, a folder outside git, a submodule) the folder
/// itself, as given, so its store is the one it always had. With
/// [`per_worktree`], always the folder itself.
pub fn store_root(project: &Path) -> PathBuf {
    if per_worktree() {
        return project.to_path_buf();
    }
    main_checkout_of(project).unwrap_or_else(|| project.to_path_buf())
}

/// For a folder inside a linked worktree (its `.git` is a file naming a
/// git directory that has a `commondir`, as `<common>/worktrees/<name>`
/// does), the same folder in the main working tree. Read from the files,
/// without running git. `None` anywhere else.
pub fn main_checkout_of(project: &Path) -> Option<PathBuf> {
    let abs = project.canonicalize().ok()?;
    let mut dir = Some(abs.as_path());
    while let Some(d) = dir {
        let g = d.join(".git");
        if g.is_dir() {
            return None;
        }
        if g.is_file() {
            let text = std::fs::read_to_string(&g).ok()?;
            let rel = text.trim().strip_prefix("gitdir:")?.trim();
            let gd = d.join(rel);
            // A submodule's git directory has no commondir: not a worktree.
            let common = std::fs::read_to_string(gd.join("commondir")).ok()?;
            let common = gd.join(common.trim());
            let common = common.canonicalize().unwrap_or(common);
            let main = if common.file_name().is_some_and(|n| n == ".git") {
                common.parent()?.to_path_buf()
            } else {
                // A bare repository, or a git directory kept apart from its
                // working tree (core.worktree).
                core_worktree(&common).unwrap_or(common)
            };
            let sub = abs.strip_prefix(d).ok()?;
            return Some(if sub.as_os_str().is_empty() { main } else { main.join(sub) });
        }
        dir = d.parent();
    }
    None
}

/// `core.worktree` in a git directory's config, resolved against it.
fn core_worktree(git_dir: &Path) -> Option<PathBuf> {
    let cfg = std::fs::read_to_string(git_dir.join("config")).ok()?;
    let mut in_core = false;
    for line in cfg.lines().map(str::trim) {
        if line.starts_with('[') {
            in_core = line.trim_matches(['[', ']']).trim().eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case("worktree") {
                let p = git_dir.join(v.trim().trim_matches('"'));
                return Some(p.canonicalize().unwrap_or(p));
            }
        }
    }
    None
}

/// The project a hook or MCP server runs for: `$CLAUDE_PROJECT_DIR`, else `cwd`.
pub fn project_dir(cwd: Option<&str>) -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_PROJECT_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    cwd.map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

pub struct StorePaths {
    pub dir: PathBuf,
    pub db: PathBuf,
    pub socket: PathBuf,
    pub spawn_lock: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
    pub config: PathBuf,
}

impl StorePaths {
    pub fn new(dir: &Path) -> StorePaths {
        StorePaths {
            dir: dir.to_path_buf(),
            db: dir.join("memory.strata"),
            socket: socket_path(dir),
            spawn_lock: dir.join("spawn.lock"),
            state: dir.join("state.json"),
            log: dir.join("daemon.log"),
            config: dir.join("config.json"),
        }
    }
}

/// `dir/engram.sock`, or a short path in a private runtime directory when
/// that would exceed the Unix socket limit (about 107 bytes). They keep the
/// old name: a daemon started before the rename listens on them.
///
/// The short path's directory must be this user's alone: another local user
/// who made `/tmp/engram-<uid>` first, and listened on the socket's
/// predictable name in it, would read every prompt and tool result the hooks
/// send and answer them with context of their own. So the directory is used
/// only when it is a real directory (not a link), owned by this user, that
/// no one else may write to (`$XDG_RUNTIME_DIR`) or open at all
/// (`/tmp/engram-<uid>`); otherwise the socket goes to `~/.cache/chloectx`.
fn socket_path(dir: &Path) -> PathBuf {
    let p = dir.join("engram.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);
    let cache = std::env::var_os("HOME").filter(|d| !d.is_empty()).map(|h| PathBuf::from(h).join(".cache/chloectx"));
    short_socket_path(dir, runtime.as_deref(), Path::new("/tmp"), cache.as_deref())
}

/// [`socket_path`]'s choice for a store directory whose own socket path is
/// too long: in `runtime` (`$XDG_RUNTIME_DIR`), else in `<tmp>/engram-<uid>`,
/// else in `cache`, the first of them that is private to this user; the
/// store directory itself when none is (binding there fails, saying the
/// path is too long, rather than listening where another user could).
pub fn short_socket_path(dir: &Path, runtime: Option<&Path>, tmp: &Path, cache: Option<&Path>) -> PathBuf {
    let name = format!("engram-{:016x}.sock", crate::embed::fnv(&dir.to_string_lossy()));
    if let Some(r) = runtime.filter(|r| check_owned_dir(r, 0o022).is_ok()) {
        return r.join(name);
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let tmp_dir = tmp.join(format!("engram-{uid}"));
    if ensure_private_dir(&tmp_dir).is_ok() {
        return tmp_dir.join(name);
    }
    if let Some(c) = cache.filter(|c| ensure_private_dir(c).is_ok()) {
        return c.join(name);
    }
    dir.join("engram.sock")
}

/// Makes `dir` (its parent must exist, or is created) a directory only this
/// user may open, and checks that it is one: created with mode 0700, or,
/// when it was there already, a real directory (not a link) owned by this
/// user, whose mode is narrowed to 0700. A directory someone else owns is
/// refused, since its owner can put anything in it.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if check_owned_dir(dir, 0o077).is_err() {
            // Ours but too open (an older ChloeCtx, or a umask): narrow it.
            check_owned_dir(dir, 0)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        check_owned_dir(dir, 0o077)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Whether `dir` is a directory (not a link to one) owned by this user with
/// none of the mode bits in `refused` set.
#[cfg(unix)]
fn check_owned_dir(dir: &Path, refused: u32) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(dir)?;
    let denied = |why: String| Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("{}: {why}", dir.display())));
    if !m.file_type().is_dir() {
        return denied("not a directory (a link or a file)".into());
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if m.uid() != uid {
        return denied(format!("owned by user {}, not by this user ({uid})", m.uid()));
    }
    if m.mode() & refused != 0 {
        return denied(format!("mode {:o} lets other users in", m.mode() & 0o777));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_owned_dir(dir: &Path, _refused: u32) -> std::io::Result<()> {
    if dir.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{}: not a directory", dir.display())))
    }
}

/// Creates a store directory readable only by its owner: it holds
/// transcripts, which can contain anything the session saw.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Makes a new directory under the system's temporary directory that only
/// its owner can open, with a name no one can guess or make first
/// (`mkdtemp`): a capsule's copy and a pack's backup of the store go there.
pub fn private_temp_dir(prefix: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut tmpl = std::env::temp_dir().join(format!("{prefix}-XXXXXX")).into_os_string().as_bytes().to_vec();
    tmpl.push(0);
    // SAFETY: a NUL-terminated buffer that mkdtemp rewrites in place.
    let p = unsafe { libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
    if p.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    tmpl.pop();
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&tmpl)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_with_the_old_folder_keeps_it() {
        let data = tempfile::tempdir().unwrap();
        let d = data.path();
        assert_eq!(home_in(d), d.join("chloectx"), "a new install");
        std::fs::create_dir(d.join("engram")).unwrap();
        assert_eq!(home_in(d), d.join("engram"), "an install from before the rename");
        std::fs::create_dir(d.join("chloectx")).unwrap();
        assert_eq!(home_in(d), d.join("chloectx"), "the new folder wins once it exists");
    }

    /// A repository laid out as `git worktree add` leaves it, written by
    /// hand (no git needed): `main/.git/` and `wt/.git` pointing at
    /// `main/.git/worktrees/wt`.
    fn repo_with_worktree(t: &Path) -> (PathBuf, PathBuf) {
        let main = t.join("main");
        let wt = t.join("wt");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        std::fs::write(main.join(".git/worktrees/wt/HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/wt").display())).unwrap();
        (main.canonicalize().unwrap(), wt.canonicalize().unwrap())
    }

    // The environment is the process's: the per-worktree switch is tested
    // in the same test, not in one running beside it.
    #[test]
    fn a_worktree_shares_its_repositorys_store() {
        let t = tempfile::tempdir().unwrap();
        let (main, wt) = repo_with_worktree(t.path());
        std::env::remove_var("CHLOECTX_PER_WORKTREE");
        // The main checkout keeps its slug; the worktree takes it.
        assert_eq!(store_root(&main), main);
        assert_eq!(project_slug(&store_root(&main)), project_slug(&main));
        assert_eq!(store_root(&wt), main);
        assert_eq!(project_slug(&store_root(&wt)), project_slug(&main));
        // A folder inside either maps to the same folder of the main checkout.
        assert_eq!(store_root(&wt.join("src")), main.join("src"));
        assert_eq!(store_root(&main.join("src")), main.join("src"));
        // Outside git: unchanged, as given.
        let plain = t.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(store_root(&plain), plain);
        // A submodule (a .git file without commondir) is not a worktree.
        let sub = t.path().join("sub");
        std::fs::create_dir_all(main.join(".git/modules/sub")).unwrap();
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join(".git"), format!("gitdir: {}\n", main.join(".git/modules/sub").display())).unwrap();
        assert_eq!(store_root(&sub), sub);
        // A relative gitdir, and a bare repository's worktree.
        let bare = t.path().join("repo.git");
        std::fs::create_dir_all(bare.join("worktrees/b")).unwrap();
        std::fs::write(bare.join("worktrees/b/commondir"), "../..\n").unwrap();
        let b = t.path().join("b");
        std::fs::create_dir(&b).unwrap();
        std::fs::write(b.join(".git"), "gitdir: ../repo.git/worktrees/b\n").unwrap();
        assert_eq!(store_root(&b), bare.canonicalize().unwrap());
        // The old behaviour on request.
        std::env::set_var("CHLOECTX_PER_WORKTREE", "1");
        assert_eq!(store_root(&wt), wt);
        assert_ne!(project_slug(&store_root(&wt)), project_slug(&main));
        std::env::remove_var("CHLOECTX_PER_WORKTREE");
    }

    /// B-13: a `/tmp/engram-<uid>` that another user made first is never
    /// where the socket goes, nor is a link standing in for it.
    #[cfg(unix)]
    #[test]
    fn the_short_socket_directory_is_this_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let store = t.path().join("store");
        let tmp = t.path().join("tmp");
        let cache = t.path().join("cache/chloectx");
        std::fs::create_dir_all(&tmp).unwrap();
        let uid = unsafe { libc::getuid() };
        let ours = tmp.join(format!("engram-{uid}"));

        // Made here: private.
        let p = short_socket_path(&store, None, &tmp, Some(&cache));
        assert_eq!(p.parent(), Some(ours.as_path()));
        assert_eq!(std::fs::metadata(&ours).unwrap().permissions().mode() & 0o777, 0o700);

        // Ours but left open: narrowed to 0700 and used.
        std::fs::set_permissions(&ours, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(short_socket_path(&store, None, &tmp, Some(&cache)).parent(), Some(ours.as_path()));
        assert_eq!(std::fs::metadata(&ours).unwrap().permissions().mode() & 0o777, 0o700);

        // A link where the directory should be: not followed.
        std::fs::remove_dir(&ours).unwrap();
        let elsewhere = t.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &ours).unwrap();
        let p = short_socket_path(&store, None, &tmp, Some(&cache));
        assert_eq!(p.parent(), Some(cache.as_path()), "{}", p.display());
        std::fs::remove_file(&ours).unwrap();

        // A runtime directory others may write to is skipped too.
        let runtime = t.path().join("run");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_ne!(short_socket_path(&store, Some(&runtime), &tmp, Some(&cache)).parent(), Some(runtime.as_path()));
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(short_socket_path(&store, Some(&runtime), &tmp, Some(&cache)).parent(), Some(runtime.as_path()));

        // Another user's directory (needs root to make one): refused.
        if uid == 0 {
            std::fs::remove_dir_all(&cache).unwrap();
            let theirs = tmp.join("engram-0");
            std::fs::create_dir_all(&theirs).unwrap();
            std::os::unix::fs::chown(&theirs, Some(65534), Some(65534)).unwrap();
            std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o700)).unwrap();
            let p = short_socket_path(&store, None, &tmp, Some(&cache));
            assert_eq!(p.parent(), Some(cache.as_path()), "another user's {} was used", theirs.display());
            assert!(ensure_private_dir(&theirs).is_err());
            // And with nowhere private, the store's own (too long) path.
            std::fs::remove_dir_all(&cache).unwrap();
            std::fs::create_dir_all(&cache).unwrap();
            std::os::unix::fs::chown(&cache, Some(65534), Some(65534)).unwrap();
            assert_eq!(short_socket_path(&store, None, &tmp, Some(&cache)), store.join("engram.sock"));
        }
    }
}
