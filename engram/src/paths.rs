//! Where a project's store, socket and logs live.

use std::path::{Path, PathBuf};

/// `$ENGRAM_HOME`, else `$XDG_DATA_HOME/engram`, else `~/.local/share/engram`.
pub fn engram_home() -> PathBuf {
    if let Ok(d) = std::env::var("ENGRAM_HOME") {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(d).join("engram");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/engram")
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

/// The store directory of a project.
pub fn store_dir(project: &Path) -> PathBuf {
    engram_home().join("projects").join(project_slug(project))
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
/// that would exceed the Unix socket limit (about 107 bytes).
fn socket_path(dir: &Path) -> PathBuf {
    let p = dir.join("engram.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let base = std::env::var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(format!("/tmp/engram-{uid}")));
    let _ = std::fs::create_dir_all(&base);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    }
    base.join(format!("engram-{:016x}.sock", crate::embed::fnv(&dir.to_string_lossy())))
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
