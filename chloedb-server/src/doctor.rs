//! `chloedb-server doctor`: what stands between this machine and the
//! guides, each line `ok`, `warn` or `FAIL` with the command that fixes it,
//! as `chloectx doctor` does for ChloeCtx:
//!
//! - this server's version against the latest release on GitHub;
//! - the Python package `chloedb`: ChloeDB, or PyPI's placeholder (which
//!   only holds the name, and which pip falls back to when a release has
//!   no wheel for the machine);
//! - the models folder and the models in it;
//! - whether port 7700, which every guide uses, is free;
//! - on Linux, the C library against what the prebuilt programs and wheels
//!   need.
//!
//! It changes nothing. Exit status 1 when a line is `FAIL`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

const REPO: &str = "chloetech-ai/chloedb";
const RELEASES: &str = "https://github.com/chloetech-ai/chloedb/releases";

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

/// One line of the report.
#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    pub name: &'static str,
    pub detail: String,
    /// The command (or what to do) that fixes it.
    pub fix: Option<String>,
}

impl Check {
    fn new(level: Level, name: &'static str, detail: impl Into<String>, fix: Option<String>) -> Check {
        Check { level, name, detail: detail.into(), fix }
    }

    pub fn line(&self) -> String {
        let tag = match self.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        match &self.fix {
            Some(f) => format!("{tag} {}: {}\n       fix: {f}", self.name, self.detail),
            None => format!("{tag} {}: {}", self.name, self.detail),
        }
    }
}

/// What `doctor` is told.
pub struct Options<'a> {
    pub models: &'a Path,
    pub port: u16,
    /// Ask GitHub for the latest release.
    pub online: bool,
    /// The Python interpreter to ask about the package (`python3`).
    pub python: &'a str,
}

/// Every check, in order.
pub async fn run(o: &Options<'_>) -> Vec<Check> {
    let mut out = vec![version(o.online).await, python(o.python), models(o.models), port(o.port)];
    if let Some(c) = glibc() {
        out.push(c);
    }
    out
}

fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse().unwrap_or(0)).collect()
}

/// The newest `chloedb-server-v<version>` among GitHub's releases (a JSON
/// list), skipping drafts and prereleases.
pub fn latest_server_release(releases: &serde_json::Value) -> Option<String> {
    releases
        .as_array()?
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(false) && !r["prerelease"].as_bool().unwrap_or(false))
        .filter_map(|r| r["tag_name"].as_str()?.strip_prefix("chloedb-server-v").map(str::to_string))
        .filter(|v| !v.contains('-'))
        .max_by_key(|v| version_key(v))
}

/// This version against the latest release.
pub fn compare_version(mine: &str, latest: Option<&str>) -> Check {
    let name = "version";
    match latest {
        None => Check::new(Level::Ok, name, format!("chloedb-server {mine} (the latest release was not looked up)"), None),
        Some(l) if version_key(mine) < version_key(l) => Check::new(
            Level::Warn,
            name,
            format!("chloedb-server {mine}, and {l} is out"),
            Some(format!("curl -fsSL https://chloetech-ai.github.io/chloedb/install.sh | sh   ({RELEASES})")),
        ),
        Some(l) if version_key(mine) > version_key(l) => {
            Check::new(Level::Ok, name, format!("chloedb-server {mine}, newer than the latest release ({l}): a build from main, which the docs follow"), None)
        }
        Some(l) => Check::new(Level::Ok, name, format!("chloedb-server {mine}, the latest release ({l})"), None),
    }
}

async fn version(online: bool) -> Check {
    let mine = env!("CARGO_PKG_VERSION");
    if !online {
        return compare_version(mine, None);
    }
    let latest = async {
        let client = crate::http_client().timeout(Duration::from_secs(5)).build().ok()?;
        let r = client.get(format!("https://api.github.com/repos/{REPO}/releases?per_page=100")).header("user-agent", "chloedb-server-doctor").send().await.ok()?;
        latest_server_release(&r.json::<serde_json::Value>().await.ok()?)
    }
    .await;
    match latest {
        Some(l) => compare_version(mine, Some(&l)),
        None => Check::new(Level::Warn, "version", format!("chloedb-server {mine}; GitHub did not say which release is the latest (offline?)"), Some(format!("see {RELEASES}, or --offline to skip this"))),
    }
}

/// What `python -c 'import chloedb'` said, as a check: its exit status,
/// what it printed and what it wrote to stderr.
pub fn python_result(status: Option<i32>, stdout: &str, stderr: &str) -> Check {
    let name = "python package";
    // PyPI has the wheels from 0.3.0: the newest that fits this machine,
    // whatever this build's version (a build from main is ahead of every
    // release, so naming its version named a release that did not exist).
    let wheels = "pip install --upgrade chloedb   (from PyPI; where pip keeps the placeholder, this machine has no wheel: build from source, docs/chloedb/python.md#from-source)".to_string();
    match status {
        None => Check::new(Level::Ok, name, "no Python 3 here (the package is optional)", None),
        Some(0) => {
            let v = stdout.trim();
            Check::new(Level::Ok, name, format!("chloedb {}", if v.is_empty() { "(a version from before __version__)" } else { v }), None)
        }
        Some(_) if stderr.contains("placeholder") => Check::new(
            Level::Fail,
            name,
            "the chloedb installed is PyPI's placeholder, which holds the name and is not ChloeDB (pip takes it when a release has no wheel for this machine)",
            Some(wheels),
        ),
        Some(_) if stderr.contains("No module named") => Check::new(Level::Ok, name, "chloedb is not installed for this Python (optional)", Some(wheels)),
        Some(_) => Check::new(Level::Warn, name, format!("import chloedb failed: {}", stderr.trim().lines().last().unwrap_or("")), Some(wheels)),
    }
}

fn python(python: &str) -> Check {
    let script = "import chloedb; print(getattr(chloedb, '__version__', ''))";
    match Command::new(python).args(["-c", script]).output() {
        Err(_) => python_result(None, "", ""),
        Ok(o) => python_result(Some(o.status.code().unwrap_or(1)), &String::from_utf8_lossy(&o.stdout), &String::from_utf8_lossy(&o.stderr)),
    }
}

fn models(dir: &Path) -> Check {
    let name = "models";
    let fetch = format!("chloedb-server fetch-model potion-retrieval-32M   (into {})", dir.display());
    let old = dir.ends_with(".local/share/strata/models");
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .map(|r| r.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect())
        .unwrap_or_default();
    found.sort();
    let note = if old { " (the folder from before the rename, kept since it is there)" } else { "" };
    if found.is_empty() {
        return Check::new(Level::Warn, name, format!("no models in {}{note}: a table whose vectors ChloeDB embeds needs its model", dir.display()), Some(fetch));
    }
    Check::new(Level::Ok, name, format!("{}{note}: {}", dir.display(), found.join(", ")), None)
}

fn port(port: u16) -> Check {
    let name = "port";
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => Check::new(Level::Ok, name, format!("{port} is free (the guides' servers listen there)"), None),
        Err(e) => Check::new(
            Level::Warn,
            name,
            format!("{port} is taken ({e}): a server from an earlier guide, or another program; the guides start theirs there"),
            Some(format!("stop what listens there (lsof -i :{port}), or give the guide's server --listen 127.0.0.1:<another port>")),
        ),
    }
}

/// The C library's version against what the prebuilt programs need.
pub fn glibc_check(version: Option<&str>) -> Check {
    let name = "glibc";
    let Some(v) = version else {
        return Check::new(Level::Warn, name, "no glibc (Alpine or another musl system): the prebuilt programs and wheels do not run here", Some("docker run ghcr.io/chloetech-ai/chloedb-server, or build from source".into()));
    };
    let k = version_key(v);
    if k < vec![2, 28] {
        Check::new(Level::Warn, name, format!("{v}: older than every prebuilt program and wheel needs (2.35; Python wheels from 0.2.1, 2.28)"), Some("docker run ghcr.io/chloetech-ai/chloedb-server, or build from source".into()))
    } else if k < vec![2, 35] {
        Check::new(Level::Warn, name, format!("{v}: the prebuilt server, JavaScript addon and release 0.2.0's Python wheels need 2.35; Python wheels from 0.2.1 (glibc 2.28, manylinux_2_28) run here"), Some("docker run ghcr.io/chloetech-ai/chloedb-server for the server".into()))
    } else {
        Check::new(Level::Ok, name, format!("{v}: the prebuilt programs and wheels run here"), None)
    }
}

fn glibc() -> Option<Check> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let out = Command::new("getconf").arg("GNU_LIBC_VERSION").output().ok();
    let v = out.filter(|o| o.status.success()).and_then(|o| String::from_utf8_lossy(&o.stdout).trim().strip_prefix("glibc ").map(str::to_string));
    Some(glibc_check(v.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_latest_server_release_skips_other_tags_drafts_and_prereleases() {
        let r = json!([
            {"tag_name": "chloedb-py-v0.9.0"},
            {"tag_name": "chloedb-server-v0.2.0"},
            {"tag_name": "chloedb-server-v0.10.0", "draft": true},
            {"tag_name": "chloedb-server-v0.3.0-rc.1"},
            {"tag_name": "chloedb-server-v0.2.10"},
            {"tag_name": "strata-server-v0.1.0"},
        ]);
        assert_eq!(latest_server_release(&r).as_deref(), Some("0.2.10"));
        assert_eq!(compare_version("0.2.1", Some("0.2.10")).level, Level::Warn);
        assert_eq!(compare_version("0.2.10", Some("0.2.10")).level, Level::Ok);
        assert!(compare_version("0.3.0", Some("0.2.10")).detail.contains("newer"));
    }

    #[test]
    fn the_placeholder_package_fails() {
        let c = python_result(Some(1), "", "ImportError: This is chloedb 0.0.2, a placeholder that holds the name");
        assert_eq!(c.level, Level::Fail);
        let fix = c.fix.unwrap();
        assert!(fix.starts_with("pip install --upgrade chloedb "), "{fix}");
        // Not a release named after this build's version, which a build
        // from main is ahead of.
        assert!(!fix.contains(env!("CARGO_PKG_VERSION")), "{fix}");
        assert_eq!(python_result(Some(0), "0.2.1\n", "").detail, "chloedb 0.2.1");
        assert_eq!(python_result(Some(1), "", "ModuleNotFoundError: No module named 'chloedb'").level, Level::Ok);
        assert_eq!(python_result(None, "", "").level, Level::Ok);
    }

    #[test]
    fn glibc_against_the_wheels() {
        assert_eq!(glibc_check(Some("2.39")).level, Level::Ok);
        assert_eq!(glibc_check(Some("2.31")).level, Level::Warn);
        assert!(glibc_check(Some("2.31")).detail.contains("2.28"));
        assert_eq!(glibc_check(Some("2.17")).level, Level::Warn);
        assert_eq!(glibc_check(None).level, Level::Warn);
    }

    #[test]
    fn models_and_port() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(models(d.path()).level, Level::Warn);
        std::fs::create_dir(d.path().join("potion-retrieval-32M")).unwrap();
        let c = models(d.path());
        assert_eq!(c.level, Level::Ok);
        assert!(c.detail.contains("potion-retrieval-32M"));
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert_eq!(port(l.local_addr().unwrap().port()).level, Level::Warn);
    }
}
