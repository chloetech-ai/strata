//! Which commit this binary is built from, how far behind its checkout's
//! upstream branch that is, and `chloectx upgrade`, which pulls the checkout
//! and installs the new build.
//!
//! ChloeCtx is built from a checkout of the repository (`install.sh`), so
//! the checkout is where a newer version comes from. A binary built
//! outside one (no `.git` at build time) knows no commit, and these checks
//! say nothing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

/// The commit the binary was built from (`build.rs`).
pub const BUILD_COMMIT: Option<&str> = option_env!("CHLOECTX_BUILD_COMMIT");

/// The paths whose commits change the binary: ChloeCtx and the database it
/// is built on.
const PATHS: [&str; 2] = ["chloectx", "chloedb"];

/// How a prebuilt binary is installed and upgraded (docs/chloectx/setup.md).
pub const RELEASE_INSTALL: &str = "curl -fsSL https://chloetech-ai.github.io/chloedb/install.sh | sh -s -- --with ctx";

/// How long a check of the checkout (without fetching) is reused.
pub const CHECK_EVERY: i64 = 6 * 3600;

/// The checkout the binary was built from: `$CHLOECTX_SOURCE`, else the
/// repository that held the crate at build time, when it is still there.
pub fn source_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("CHLOECTX_SOURCE").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf))?;
    (dir.join("chloectx/Cargo.toml").is_file() && dir.join(".git").exists()).then_some(dir)
}

/// Runs git in `dir`, with a deadline; its trimmed stdout when it succeeds.
pub fn git(dir: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let mut out = String::new();
                std::io::Read::read_to_string(child.stdout.as_mut()?, &mut out).ok()?;
                return st.success().then(|| out.trim().to_string());
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// The branch the checkout follows (`@{upstream}`), else `origin/main`.
pub fn upstream(src: &Path) -> String {
    git(src, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"], Duration::from_secs(5)).filter(|u| u.contains('/')).unwrap_or_else(|| "origin/main".into())
}

/// How this build stands against the checkout's upstream branch.
#[derive(Clone, Debug, PartialEq)]
pub struct Standing {
    /// The build's commit.
    pub build: String,
    /// The upstream branch (`origin/main`).
    pub upstream: String,
    /// Commits on the upstream branch since the build that change ChloeCtx
    /// or ChloeDB.
    pub behind: u64,
    /// The upstream branch was fetched by this check.
    pub fetched: bool,
    /// The fetch this check asked for failed (offline, an unreachable or
    /// silent remote): `behind` counts the commits of the last fetch.
    pub fetch_error: Option<String>,
    /// When ChloeCtx last fetched the upstream branch, if known.
    pub fetched_at: Option<i64>,
}

impl Standing {
    pub fn json(&self) -> Json {
        json!({"build": self.build, "upstream": self.upstream, "behind": self.behind})
    }

    fn of(v: &Json) -> Option<Standing> {
        Some(Standing {
            build: v["build"].as_str()?.into(),
            upstream: v["upstream"].as_str()?.into(),
            behind: v["behind"].as_u64()?,
            fetched: false,
            fetch_error: None,
            fetched_at: v["fetched_at"].as_i64(),
        })
    }

    /// One line for a person: where this build stands. When the fetch
    /// failed it says so, and that the count is as of the last fetch
    /// (CTX-4: it said "up to date" offline).
    pub fn line(&self) -> String {
        let short = &self.build[..self.build.len().min(10)];
        let standing = match self.behind {
            0 => format!("built from {short}, up to date with {}", self.upstream),
            1 => format!("built from {short}; 1 commit on {} changes ChloeCtx since", self.upstream),
            n => format!("built from {short}; {n} commits on {} change ChloeCtx since", self.upstream),
        };
        match &self.fetch_error {
            Some(e) => {
                let last = match self.fetched_at {
                    Some(t) => format!("as of its last fetch, {}", crate::recall::fmt_ago(t, crate::now())),
                    None => "as of its last fetch".to_string(),
                };
                format!("could not fetch {} ({e}), so not checked: {standing} {last}", self.upstream)
            }
            None if self.behind > 0 => format!("{standing}; `chloectx upgrade` installs {}", if self.behind == 1 { "it" } else { "them" }),
            None => standing,
        }
    }
}

/// How long `upgrade --check` waits for the fetch.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `doctor` waits for it: a check should not hang on a remote
/// that accepts and never answers (CTX-4: doctor took 21 s).
pub const DOCTOR_FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// `git fetch` of `branch` from `remote` in `dir` within `timeout`; why not
/// when it failed.
pub fn fetch(dir: &Path, remote: &str, branch: &str, timeout: Duration) -> std::result::Result<(), String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["fetch", "--quiet", remote, branch])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", std::env::var("GIT_SSH_COMMAND").unwrap_or_else(|_| "ssh -o BatchMode=yes -o ConnectTimeout=5".into()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(_)) => {
                // The remote's helper may still hold stderr; do not wait on it.
                let t = Instant::now() + Duration::from_millis(500);
                while !reader.is_finished() && Instant::now() < t {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let said = if reader.is_finished() { reader.join().unwrap_or_default() } else { String::new() };
                let line = said.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("git fetch failed").to_string();
                return Err(crate::llm::clip(&line, 200));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer from {remote} in {} s", timeout.as_secs()));
            }
        }
    }
}

/// Where `build` stands in the checkout `src`, fetching the upstream branch
/// first when `fetch` is a timeout (which may fail offline: the last
/// fetch's view is used then, and the standing says the fetch failed).
pub fn standing(src: &Path, build: &str, fetch: Option<Duration>) -> Option<Standing> {
    let up = upstream(src);
    let mut fetched = false;
    let mut fetch_error = None;
    if let Some(timeout) = fetch {
        match up.split_once('/') {
            Some((remote, branch)) => match self::fetch(src, remote, branch, timeout) {
                Ok(()) => fetched = true,
                Err(e) => fetch_error = Some(e),
            },
            None => fetch_error = Some(format!("{up} names no remote")),
        }
    }
    let range = format!("{build}..{up}");
    let mut args = vec!["rev-list", "--count", range.as_str(), "--"];
    args.extend(PATHS);
    let behind = git(src, &args, Duration::from_secs(10))?.parse().ok()?;
    Some(Standing { build: build.to_string(), upstream: up, behind, fetched, fetch_error, fetched_at: None })
}

fn cache_path(home: &Path) -> PathBuf {
    home.join("version-check.json")
}

fn read_cache(home: &Path) -> Json {
    std::fs::read_to_string(cache_path(home)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null)
}

fn write_cache(home: &Path, v: &Json) {
    let p = cache_path(home);
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(tmp, p);
    }
}

/// The last check of this build, when it is newer than `max_age` seconds.
pub fn cached(home: &Path, max_age: i64) -> Option<Standing> {
    let v = read_cache(home);
    let fresh = crate::now() - v["checked_at"].as_i64()? <= max_age;
    (fresh && BUILD_COMMIT == v["build"].as_str()).then(|| Standing::of(&v)).flatten()
}

/// Checks the checkout again (fetching within `fetch` when it is a
/// timeout) and keeps the result for [`cached`]; `None` when there is no
/// commit or checkout to check.
pub fn check(home: &Path, fetch: Option<Duration>) -> Option<Standing> {
    let build = BUILD_COMMIT?;
    let s = standing(&source_dir()?, build, fetch)?;
    Some(remember(home, s))
}

/// Keeps a standing for [`cached`], with when the upstream was last fetched.
fn remember(home: &Path, mut s: Standing) -> Standing {
    let mut v = read_cache(home);
    if v["build"].as_str() != Some(s.build.as_str()) {
        let fetched_at = v["fetched_at"].clone();
        v = json!({"fetched_at": fetched_at});
    }
    for (k, x) in s.json().as_object().unwrap() {
        v[k] = x.clone();
    }
    v["checked_at"] = json!(crate::now());
    if s.fetched {
        v["fetched_at"] = json!(crate::now());
    }
    s.fetched_at = v["fetched_at"].as_i64();
    write_cache(home, &v);
    s
}

/// How often `chloectx ui` fetches the upstream branch on its own: the page
/// asks on each load, and asking again within this many seconds reuses the
/// last fetch.
pub const UI_FETCH_EVERY: i64 = 600;

/// What `chloectx ui` shows of this build: its version and commit, the
/// checkout it came from, and how it stands against the checkout's
/// upstream branch. `check` checks again, fetching the upstream branch
/// unless it was fetched in the last [`UI_FETCH_EVERY`] seconds (`force`:
/// fetching anyway); else the last check is shown, or one made now without
/// fetching. `standing` is null for a build that knows no commit or
/// checkout.
pub fn summary(home: &Path, check: bool, force: bool) -> Json {
    let fetched_at = read_cache(home)["fetched_at"].as_i64().unwrap_or(0);
    let s = if check {
        let fetch = force || crate::now() - fetched_at > UI_FETCH_EVERY;
        self::check(home, fetch.then_some(FETCH_TIMEOUT))
    } else {
        cached(home, CHECK_EVERY).or_else(|| self::check(home, None))
    };
    let v = read_cache(home);
    json!({
        "version": env!("CARGO_PKG_VERSION"), "build": BUILD_COMMIT, "source": source_dir(),
        "standing": s.as_ref().map(Standing::json), "line": s.as_ref().map(Standing::line),
        "fetch_error": s.as_ref().and_then(|s| s.fetch_error.clone()),
        "checked_at": v["checked_at"], "fetched_at": v["fetched_at"],
    })
}

/// Whether to tell the person about a newer version now (at most once a
/// day): marks it told.
pub fn notify_due(home: &Path) -> bool {
    let mut v = read_cache(home);
    if v["build"].as_str() != BUILD_COMMIT || v["behind"].as_u64().unwrap_or(0) == 0 {
        return false;
    }
    if crate::now() - v["notified_at"].as_i64().unwrap_or(0) < 24 * 3600 {
        return false;
    }
    v["notified_at"] = json!(crate::now());
    write_cache(home, &v);
    true
}

// ---------------------------------------------------------------- upgrade

pub struct UpgradeOptions {
    /// Only say where this build stands.
    pub check: bool,
    /// Build the checkout as it is, without pulling.
    pub no_pull: bool,
    /// Build and install even when the build is the checkout's commit.
    pub force: bool,
}

/// `chloectx upgrade`: pulls the checkout the binary was built from (fast
/// forward only), installs the new build over this one with `install.sh
/// --no-wire`, wires the user-level install again (new hooks a version
/// adds), moves the store folder from its name before the rename, and
/// runs `doctor` with the new binary.
pub fn upgrade(project: &Path, o: &UpgradeOptions) -> i32 {
    let Some(src) = source_dir() else {
        // A prebuilt binary (the release installer's) has no checkout:
        // the same installer upgrades it, with no Rust (CTX-5).
        eprintln!(
            "chloectx upgrade: this binary was not built from a checkout on this machine (a prebuilt release, or its checkout is gone).\n\
             To upgrade a prebuilt one, run the installer again; it replaces {} and keeps your stores and settings:\n\
             \x20 {RELEASE_INSTALL}\n\
             then `chloectx install --user` (or --project DIR) again if a release adds hooks, and `chloectx doctor`.\n\
             Built from a checkout: set CHLOECTX_SOURCE to it and run chloectx upgrade again.",
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "chloectx".into())
        );
        return 1;
    };
    let build = BUILD_COMMIT.unwrap_or("");
    println!("ChloeCtx {}, built from {} in {}", env!("CARGO_PKG_VERSION"), if build.is_empty() { "an unknown commit" } else { &build[..build.len().min(10)] }, src.display());
    if !build.is_empty() {
        match standing(&src, build, Some(FETCH_TIMEOUT)) {
            Some(s) => {
                let s = remember(&crate::paths::home(), s);
                println!("{}", s.line());
                if o.check {
                    // A check that could not fetch did not check.
                    return if s.fetch_error.is_some() { 1 } else { 0 };
                }
            }
            None if o.check => {
                eprintln!("chloectx upgrade: could not compare the build with the checkout's upstream branch");
                return 1;
            }
            None => {}
        }
    } else if o.check {
        println!("the build's commit is unknown; `chloectx upgrade` builds the checkout");
        return 0;
    }
    let run = |cmd: &mut Command| -> bool { cmd.stdin(Stdio::null()).status().map(|s| s.success()).unwrap_or(false) };
    if !o.no_pull {
        if git(&src, &["symbolic-ref", "-q", "HEAD"], Duration::from_secs(5)).is_none() {
            eprintln!("chloectx upgrade: {} is not on a branch (a detached HEAD); check out main there, or pass --no-pull to build it as it is", src.display());
            return 1;
        }
        println!("==> git pull --ff-only in {}", src.display());
        if !run(Command::new("git").arg("-C").arg(&src).args(["pull", "--ff-only", "--quiet"])) {
            eprintln!("chloectx upgrade: git pull --ff-only failed in {} (local commits or changes in the way?); fix it there, or pass --no-pull", src.display());
            return 1;
        }
    }
    let head = git(&src, &["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap_or_default();
    let up_to_date = !o.force && !build.is_empty() && head == build;
    // Daemons of the old build stop; they start again, new, on the next
    // hook. An up-to-date build stops them only to move the stores, which
    // `doctor` sends the user here for whether or not there is a new build.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("chloectx"));
    if !up_to_date || old_home_to_move() {
        let _ = Command::new(&exe).args(["stop", "--all"]).stdout(Stdio::null()).status();
        match move_old_home() {
            Ok(Some((from, to))) => println!("==> moved the stores from {} to {} (its name before the rename is a link to it)", from.display(), to.display()),
            Ok(None) => {}
            Err(e) => println!("==> the stores stay where they are: {e}"),
        }
    }
    if up_to_date {
        println!("ChloeCtx is up to date (the checkout is at the commit this binary was built from).");
        return 0;
    }
    let bin_dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let prefix = if bin_dir.file_name().is_some_and(|n| n == "bin") { bin_dir.parent().map(Path::to_path_buf).unwrap_or_default() } else { bin_dir.clone() };
    println!("==> building and installing (install.sh --no-wire --prefix {}); the first build takes several minutes", prefix.display());
    if !run(Command::new("sh").arg(src.join("install.sh")).args(["--no-wire", "--prefix"]).arg(&prefix)) {
        eprintln!("chloectx upgrade: install.sh failed (see above); this binary is unchanged");
        return 1;
    }
    let new = prefix.join("bin/chloectx");
    let new = if new.is_file() { new } else { exe.clone() };
    // A version may add hook events: the user-level install is written
    // again (it keeps the settings' distillation model).
    let user_hooks = std::fs::read_to_string(crate::paths::user_settings()).map(|s| s.contains(" hook --user ")).unwrap_or(false);
    if user_hooks {
        let _ = run(Command::new(&new).args(["install", "--user"]).stdout(Stdio::null()));
    }
    println!("==> checking");
    Command::new(&new).arg("--project").arg(project).arg("doctor").status().ok().and_then(|s| s.code()).unwrap_or(1)
}

/// Moves `<data>/engram`, the store folder's name before the rename, to
/// `<data>/chloectx` and leaves a link by the old name. Only when
/// `$CHLOECTX_HOME` is not set, the old folder is the one in use, and no
/// daemon answers in it. Returns what moved where.
pub fn move_old_home() -> std::result::Result<Option<(PathBuf, PathBuf)>, String> {
    if std::env::var_os("CHLOECTX_HOME").is_some_and(|h| !h.is_empty()) {
        return Ok(None);
    }
    move_old_home_in(&crate::paths::data_dir())
}

/// Whether [`move_old_home`] has a folder to move (it may still refuse
/// while a daemon runs in it).
fn old_home_to_move() -> bool {
    if std::env::var_os("CHLOECTX_HOME").is_some_and(|h| !h.is_empty()) {
        return false;
    }
    let data = crate::paths::data_dir();
    let old = crate::paths::home_in(&data);
    old.file_name().is_some_and(|n| n == crate::paths::OLD_HOME)
        && std::fs::symlink_metadata(&old).is_ok_and(|m| !m.file_type().is_symlink())
        && !data.join(crate::paths::HOME).exists()
}

/// [`move_old_home`] in the data folder `data`.
pub fn move_old_home_in(data: &Path) -> std::result::Result<Option<(PathBuf, PathBuf)>, String> {
    let old = crate::paths::home_in(data);
    if old.file_name().is_none_or(|n| n != crate::paths::OLD_HOME) || std::fs::symlink_metadata(&old).map(|m| m.file_type().is_symlink()).unwrap_or(true) {
        return Ok(None);
    }
    let new = data.join(crate::paths::HOME);
    if new.exists() {
        return Ok(None);
    }
    if let Some(s) = crate::ui::list_stores(&old).iter().find(|s| std::os::unix::net::UnixStream::connect(crate::paths::StorePaths::new(&s.dir).socket).is_ok()) {
        return Err(format!("a daemon still runs in {} (run `chloectx stop --all` and try again)", s.dir.display()));
    }
    std::fs::rename(&old, &new).map_err(|e| format!("{} to {}: {e}", old.display(), new.display()))?;
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(crate::paths::HOME, &old);
    Ok(Some((old, new)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let st = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(st.success(), "git {args:?}");
    }

    fn commit(dir: &Path, file: &str) -> String {
        let p = dir.join(file);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("{}\n", crate::now())).unwrap();
        sh(dir, &["add", "-A"]);
        sh(dir, &["commit", "-q", "-m", file]);
        git(dir, &["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap()
    }

    /// Commits upstream count when they change ChloeCtx or ChloeDB, and
    /// only once they are fetched.
    #[test]
    fn counts_the_upstream_commits_that_change_chloectx() {
        if git(Path::new("."), &["--version"], Duration::from_secs(5)).is_none() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let up = t.path().join("up");
        std::fs::create_dir(&up).unwrap();
        sh(&up, &["init", "-q", "-b", "main"]);
        let built = commit(&up, "chloectx/Cargo.toml");
        sh(t.path(), &["clone", "-q", up.to_str().unwrap(), "co"]);
        let co = t.path().join("co");
        let s = standing(&co, &built, None).unwrap();
        assert_eq!((s.behind, s.upstream.as_str()), (0, "origin/main"));
        assert!(s.line().contains("up to date"), "{}", s.line());

        commit(&up, "chloectx/src/a.rs");
        commit(&up, "docs/a.md");
        commit(&up, "chloedb/src/b.rs");
        assert_eq!(standing(&co, &built, None).unwrap().behind, 0, "not fetched yet");
        let s = standing(&co, &built, Some(FETCH_TIMEOUT)).unwrap();
        assert_eq!(s.behind, 2, "the docs commit does not count");
        assert!(s.line().contains("2 commits on origin/main") && s.line().contains("chloectx upgrade"), "{}", s.line());
    }

    /// CTX-4: a fetch that fails, or a remote that never answers, is said
    /// to have failed (not "up to date"), within the timeout.
    #[test]
    fn a_failed_fetch_is_not_up_to_date() {
        if git(Path::new("."), &["--version"], Duration::from_secs(5)).is_none() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let up = t.path().join("up");
        std::fs::create_dir(&up).unwrap();
        sh(&up, &["init", "-q", "-b", "main"]);
        let built = commit(&up, "chloectx/Cargo.toml");
        sh(t.path(), &["clone", "-q", up.to_str().unwrap(), "co"]);
        let co = t.path().join("co");
        // The remote is gone.
        sh(&co, &["remote", "set-url", "origin", t.path().join("nowhere").to_str().unwrap()]);
        let s = standing(&co, &built, Some(FETCH_TIMEOUT)).unwrap();
        assert!(!s.fetched && s.fetch_error.is_some(), "{s:?}");
        assert!(s.line().starts_with("could not fetch origin/main") && !s.line().starts_with("built from"), "{}", s.line());
        assert!(s.line().contains("as of its last fetch"), "{}", s.line());

        // A remote that accepts and never answers: given up at the timeout.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        sh(&co, &["remote", "set-url", "origin", &format!("http://127.0.0.1:{port}/x.git")]);
        let t0 = Instant::now();
        let s = standing(&co, &built, Some(Duration::from_secs(2))).unwrap();
        assert!(t0.elapsed() < Duration::from_secs(6), "{:?}", t0.elapsed());
        assert!(s.fetch_error.as_deref().is_some_and(|e| e.contains("no answer")), "{s:?}");
        drop(l);
    }

    /// The old store folder moves to the new name, once, and the old name
    /// still leads to it.
    #[test]
    fn the_old_store_folder_moves_once() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path();
        assert_eq!(move_old_home_in(data).unwrap(), None, "nothing to move");
        std::fs::create_dir_all(data.join("engram/projects/app-1")).unwrap();
        std::fs::write(data.join("engram/projects/app-1/project.txt"), "/w/app\n").unwrap();
        let (from, to) = move_old_home_in(data).unwrap().unwrap();
        assert_eq!((from, to.clone()), (data.join("engram"), data.join("chloectx")));
        assert!(to.join("projects/app-1/project.txt").is_file());
        assert!(data.join("engram/projects/app-1/project.txt").is_file(), "the old name is a link to the new folder");
        assert_eq!(crate::paths::home_in(data), data.join("chloectx"));
        assert_eq!(move_old_home_in(data).unwrap(), None, "moved already");
    }
}
