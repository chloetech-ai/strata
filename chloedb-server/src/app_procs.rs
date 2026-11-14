//! The apps a server starts itself: on a server of its own or a
//! coordinator, `chloedb-server` runs the crawler (`chloedb-crawl serve`),
//! Sheets (`chloedb-sheets serve`) and ChloeCtx (`chloectx app`) as child
//! processes when their programs are installed beside it, so that the
//! command center shows them with nothing more to start
//! (`--start-apps`, default `auto`; `off` starts none).
//!
//! Each child
//!
//! - listens on a loopback port of its own, which only this server calls
//!   ([`crate::apps`] forwards `/v1/apps/<name>/...` to it): the server
//!   binds it, keeps it for its own life, and hands the child the
//!   listening socket as file descriptor 3 ([`LISTEN_FD`]), so that no
//!   other process can take the port between the server choosing it and
//!   the app listening (at its first start or after a crash);
//! - takes requests only with a token of its own, made at each start, which
//!   the server sends it and no one else holds;
//! - calls the server back with a key of its own, scoped to what the app
//!   does ([`AppKeys`]), never with the server's token: its environment is
//!   the server's with every credential taken out ([`child_env`]);
//! - runs in a process group of its own (a Ctrl-C reaches the server, which
//!   then stops it) and, on Linux, ends when the server does;
//! - is started again when it exits, after 1 s, then up to a minute between
//!   tries, and stopped with SIGTERM (10 s, then SIGKILL) when the server
//!   stops.
//!
//! Its output goes to the server's log, one line at a time, under `app`,
//! and the last lines are kept for the command center (`GET /v1/apps`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::telemetry::now_ms;

/// The apps a server can start, by the name the command center lists them
/// under and the program that serves them.
pub const KNOWN: &[(&str, &str)] = &[("crawler", "chloedb-crawl"), ("sheets", "chloedb-sheets"), ("chloectx", "chloectx")];

/// The lines of an app's output kept for `GET /v1/apps`.
const TAIL: usize = 40;

/// The file descriptor an app's child finds its listening socket on, and
/// the setting that says so (`CHLOEDB_LISTEN_FD=3`): the programs the
/// server starts listen on it rather than binding a port themselves.
pub const LISTEN_FD: i32 = 3;
pub const LISTEN_FD_ENV: &str = "CHLOEDB_LISTEN_FD";

/// One app the server starts: what [`plan`] chose.
#[derive(Clone, Debug)]
pub struct Plan {
    pub name: String,
    pub bin: PathBuf,
    /// The loopback port it listens on.
    pub port: u16,
    /// The socket bound to `port`, listening, which the server holds while
    /// it runs and each start of the child inherits ([`LISTEN_FD`]).
    /// `None` only in tests of [`command`].
    pub listener: Option<Arc<std::net::TcpListener>>,
    /// What the server sends it (`Authorization: Bearer`), and it requires.
    pub token: String,
}

impl Plan {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Why the server starts no apps of its own, if it should not: a
/// replica, a shard with a coordinator beside it, a pod the Kubernetes
/// operator runs (it runs the apps as Deployments of their own), a server
/// over TLS (the children would not trust its certificate on 127.0.0.1).
pub struct Role {
    pub replica: bool,
    /// Given a cluster token but no shards or replicas: a shard of a
    /// coordinator, which runs the apps.
    pub shard: bool,
    pub beside_coordinator: bool,
    pub kube: bool,
    pub tls: bool,
}

impl Role {
    fn why_not(&self) -> Option<&'static str> {
        if self.replica {
            Some("a replica")
        } else if self.shard {
            Some("a shard (a cluster token, no --shards): its coordinator starts them")
        } else if self.beside_coordinator {
            Some("a shard with its coordinator beside it")
        } else if self.kube {
            Some("run by Kubernetes, where the operator runs the apps (spec.crawler, spec.sheets, spec.chloectx)")
        } else if self.tls {
            Some("serving over TLS")
        } else {
            None
        }
    }
}

/// The apps to start, from `--start-apps` (`auto`, `off`, or names:
/// `crawler,sheets`): with `auto`, each of [`KNOWN`] whose program is in
/// the server's own directory (an install puts them together; a build's
/// `target/` has none), unless `taken` (an `--app` of that name) or the
/// server's role says not to; named ones are looked for on the `PATH` too,
/// and one missing is an error.
pub fn plan(choice: &str, taken: &[String], role: &Role, own_dir: Option<&Path>) -> Result<Vec<Plan>, String> {
    let choice = choice.trim();
    if choice.is_empty() || choice == "off" {
        return Ok(Vec::new());
    }
    let auto = choice == "auto";
    let names: Vec<&str> = if auto { KNOWN.iter().map(|(n, _)| *n).collect() } else { choice.split(',').map(str::trim).filter(|s| !s.is_empty()).collect() };
    if let Some(why) = role.why_not() {
        if !auto {
            return Err(format!("--start-apps {choice}: this server is {why}, which starts no apps of its own"));
        }
        tracing::info!("starts no apps of its own: this server is {why}");
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for name in names {
        let Some((_, program)) = KNOWN.iter().find(|(n, _)| *n == name) else {
            return Err(format!("--start-apps: no app {name:?} (the server starts {}, or auto, or off)", KNOWN.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")));
        };
        if taken.iter().any(|t| t == name) {
            if !auto {
                return Err(format!("--start-apps {name} and --app {name}=...: one or the other"));
            }
            continue;
        }
        let bin = match find(program, own_dir, !auto) {
            Some(b) => b,
            None if auto => continue,
            None => return Err(format!("--start-apps {name}: no {program} beside chloedb-server or on the PATH")),
        };
        let (listener, port) = listener()?;
        out.push(Plan { name: name.to_string(), bin, port, listener: Some(Arc::new(listener)), token: random_token()? });
    }
    Ok(out)
}

/// `program` in `own_dir`, else (with `path`) on the PATH.
fn find(program: &str, own_dir: Option<&Path>, path: bool) -> Option<PathBuf> {
    let here = own_dir.map(|d| d.join(program)).filter(|p| p.is_file());
    if here.is_some() || !path {
        return here;
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|d| d.join(program)).find(|p| p.is_file())
}

/// 24 random bytes, in hex.
fn random_token() -> Result<String, String> {
    use std::io::Read;
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| format!("no random bytes for an app's token: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// A loopback socket listening on a port of its own, and the port: kept,
/// never closed and bound again, so that the port is the app's for as
/// long as the server runs (audit N-P1: a port chosen and released could
/// be taken by any local process, which would then get the app's token
/// and serve the command center's pages for it).
fn listener() -> Result<(std::net::TcpListener, u16), String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no loopback port for an app: {e}"))?;
    let port = l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())?;
    Ok((l, port))
}

/// How an app gets the key it calls the server with: `Ok(None)` on an
/// open server, where a request needs no credential.
pub trait AppKeys: Send + Sync {
    fn key_for(&self, app: &str) -> Result<Option<String>, String>;
    /// The app stopped for good: its key ends.
    fn end(&self, _app: &str) {}
}

/// What each app's key may touch: the tables it keeps, at the admin scope
/// (it makes them), and the ones it reads.
pub fn grants(app: &str) -> &'static [(&'static str, crate::keys::Scope)] {
    use crate::keys::Scope::{Admin, Read};
    match app {
        "crawler" => &[("crawl_*", Admin)],
        // Sheets opens any table as a live sheet.
        "sheets" => &[("sheets_*", Admin), ("*", Read)],
        // The Inspector reads what chloectx sync copied: a table per
        // project, and the one projects shared before 0.3.4.
        "chloectx" => &[("ctx_*", Admin), ("chloectx_memory", Read), ("chloectx_mem_*", Read)],
        _ => &[],
    }
}

/// The server's keys, which make each app its own ([`crate::keys`]): an
/// ephemeral key limited to [`grants`], kept in memory only, never in the
/// keys file nor merged with other servers, and made again (the old one
/// ended) at each start of the app. It does not close an open server.
pub struct ServerKeys {
    pub keys: Arc<crate::keys::Keys>,
}

impl AppKeys for ServerKeys {
    fn key_for(&self, app: &str) -> Result<Option<String>, String> {
        let grants = grants(app).iter().map(|(t, s)| crate::keys::Grant::new(t, *s)).collect::<Result<Vec<_>, _>>().map_err(|e| e.message)?;
        if grants.is_empty() {
            return Err(format!("no grants for an app named {app}"));
        }
        self.keys.app_key(app, grants).map(Some).map_err(|e| e.message)
    }

    fn end(&self, app: &str) {
        self.keys.end_app_key(app);
    }
}

/// The settings a child keeps of the server's environment: none that holds
/// a credential of the server's (its token, its cluster token, its key,
/// the apps' token) reaches an app. What it needs to run (the PATH, the
/// home, the locale, proxies and CA files, where ChloeCtx keeps its
/// models, the Anthropic key ChloeCtx and the crawler's model calls use)
/// does.
fn keep_env(name: &str) -> bool {
    const KEEP: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TZ", "TMPDIR", "TERM",
        "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR",
        "SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "no_proxy", "all_proxy",
        "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "CHLOEDB_CRAWL_CONTACT", "CHLOECTX_HOME", "CHLOECTX_EMBEDDER", "CHLOECTX_DISTILL_MODEL",
    ];
    KEEP.contains(&name) || name.starts_with("LC_")
}

/// The environment of an app's child: [`keep_env`] of the server's.
pub fn child_env() -> Vec<(String, String)> {
    std::env::vars().filter(|(k, _)| keep_env(k)).collect()
}

/// The arguments and settings that make `plan`'s program serve the app
/// against the server at `server`, with `key`.
fn command(p: &Plan, server: &str, key: Option<&str>) -> (Vec<String>, Vec<(&'static str, String)>) {
    let listen = format!("127.0.0.1:{}", p.port);
    // CHLOEDB_APP_OF: the server it is an app of, which lists it already.
    let mut env: Vec<(&'static str, String)> = vec![("CHLOEDB_START_APPS", "off".into()), ("CHLOEDB_APP_OF", server.to_string())];
    if p.listener.is_some() {
        env.push((LISTEN_FD_ENV, LISTEN_FD.to_string()));
    }
    if let Some(k) = key {
        env.push(("CHLOEDB_KEY", k.to_string()));
    }
    let args: Vec<String> = match p.name.as_str() {
        "crawler" => {
            env.push(("CHLOEDB_CRAWL_TOKEN", p.token.clone()));
            // Its tables are crawl_<host>: the ones its key may make.
            vec!["serve".into(), "--listen".into(), listen, "--strata".into(), server.into(), "--grow".into(), "off".into(), "--table-prefix".into(), "crawl_".into()]
        }
        "sheets" => {
            env.push(("CHLOEDB_SHEETS_TOKEN", p.token.clone()));
            vec!["serve".into(), "--listen".into(), listen, "--strata".into(), server.into()]
        }
        "chloectx" => {
            env.push(("CHLOECTX_APP_TOKEN", p.token.clone()));
            vec!["app".into(), "--server".into(), server.into(), "--host".into(), "127.0.0.1".into(), "--port".into(), p.port.to_string()]
        }
        other => unreachable!("no app {other}"),
    };
    (args, env)
}

/// What a running app's process is doing, for `GET /v1/apps`.
#[derive(Default)]
struct State {
    pid: Option<u32>,
    started_ms: u64,
    /// Times it was started again after it exited.
    restarts: u32,
    /// How it last ended, or why it could not start.
    last_exit: Option<String>,
    tail: VecDeque<String>,
}

/// An app the server starts, and its process's state.
pub struct Proc {
    pub plan: Plan,
    state: Mutex<State>,
    stopping: AtomicBool,
    /// Woken by [`Self::stop`], for a supervisor waiting to start again.
    stopped: tokio::sync::Notify,
    /// Whether the supervisor has ended (it ends only once stopping).
    done: AtomicBool,
}

impl Proc {
    pub fn new(plan: Plan) -> Arc<Proc> {
        Arc::new(Proc { plan, state: Mutex::new(State::default()), stopping: AtomicBool::new(false), stopped: tokio::sync::Notify::new(), done: AtomicBool::new(false) })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn line(&self, line: String) {
        let mut s = self.state();
        if s.tail.len() == TAIL {
            s.tail.pop_front();
        }
        s.tail.push_back(line);
    }

    /// `{program, pid, running, started_ms, restarts, last_exit, tail}`.
    pub fn json(&self) -> Json {
        let s = self.state();
        json!({
            "program": self.plan.bin.file_name().map(|f| f.to_string_lossy().into_owned()),
            "pid": s.pid, "running": s.pid.is_some(), "started_ms": s.started_ms,
            "restarts": s.restarts, "last_exit": s.last_exit, "tail": s.tail,
        })
    }

    /// Runs the app until [`Self::stop`]: starts it, waits for it to end,
    /// and starts it again (after 1 s, doubling to a minute while it keeps
    /// ending within a minute of its start).
    pub async fn supervise(self: Arc<Proc>, server: String, keys: Arc<dyn AppKeys>) {
        let mut wait = Duration::from_secs(1);
        let mut starts = 0u32;
        self.prepare().await;
        while !self.stopping.load(Ordering::SeqCst) {
            let t0 = Instant::now();
            if starts > 0 {
                self.state().restarts = starts;
            }
            starts += 1;
            let ended = match self.start(&server, keys.as_ref()) {
                Ok(child) => self.wait(child).await,
                Err(e) => e,
            };
            self.state().last_exit = Some(ended.clone());
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            tracing::warn!(app = %self.plan.name, "{ended}; starting it again in {} s", wait.as_secs());
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.stopped.notified() => {}
            }
            wait = if t0.elapsed() > Duration::from_secs(60) { Duration::from_secs(1) } else { (wait * 2).min(Duration::from_secs(60)) };
        }
        keys.end(&self.plan.name);
        self.done.store(true, Ordering::SeqCst);
    }

    /// What runs once before the app's first start: ChloeCtx fetches its
    /// embedding model (129 MB, once; kept files are checked, not fetched
    /// again), which `chloectx app` cannot start without.
    async fn prepare(&self) {
        if self.plan.name != "chloectx" || self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let mut cmd = Command::new(&self.plan.bin);
        cmd.arg("fetch-model").env_clear().envs(child_env()).stdin(Stdio::null()).kill_on_drop(true);
        match cmd.output().await {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
                tracing::warn!(app = %self.plan.name, "chloectx fetch-model failed ({}): {why}", o.status);
                self.line(format!("chloectx fetch-model: {why}"));
            }
            Err(e) => tracing::warn!(app = %self.plan.name, "cannot run chloectx fetch-model: {e}"),
        }
    }

    fn start(self: &Arc<Proc>, server: &str, keys: &dyn AppKeys) -> Result<Child, String> {
        let key = keys.key_for(&self.plan.name).map_err(|e| format!("no key for the app: {e}"))?;
        let (args, env) = command(&self.plan, server, key.as_deref());
        let mut cmd = Command::new(&self.plan.bin);
        cmd.args(&args).env_clear().envs(child_env()).envs(env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(unix)]
        let listen_fd = self.plan.listener.as_ref().map(|l| std::os::fd::AsRawFd::as_raw_fd(l.as_ref()));
        #[cfg(unix)]
        // SAFETY: async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                // The listening socket, at LISTEN_FD and kept open across
                // exec (dup2 clears close-on-exec on the copy; on the
                // descriptor itself, if it is LISTEN_FD already, fcntl).
                if let Some(fd) = listen_fd {
                    if fd == LISTEN_FD {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    } else if libc::dup2(fd, LISTEN_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // A group of its own: a Ctrl-C in the server's terminal
                // reaches the server, which stops the app, rather than
                // both at once (which the app would be started again for).
                libc::setpgid(0, 0);
                // It ends when the server does, killed or not.
                #[cfg(target_os = "linux")]
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", self.plan.bin.display()))?;
        let pid = child.id();
        tracing::info!(app = %self.plan.name, pid = ?pid, url = %self.plan.url(), program = %self.plan.bin.display(), "started the app");
        {
            let mut s = self.state();
            s.pid = pid;
            s.started_ms = now_ms();
        }
        // Stopped meanwhile: `stop` set `stopping` before it read the pid,
        // so either it saw this one or this sees it stopping.
        if self.stopping.load(Ordering::SeqCst) {
            signal(pid, libc::SIGTERM);
        }
        let outs: [Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>; 2] = [child.stdout.take().map(|o| Box::new(o) as _), child.stderr.take().map(|e| Box::new(e) as _)];
        for out in outs.into_iter().flatten() {
            let me = self.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::info!(app = %me.plan.name, "{l}");
                    me.line(l);
                }
            });
        }
        Ok(child)
    }

    /// Waits for the child to end: how it did.
    async fn wait(&self, mut child: Child) -> String {
        let r = child.wait().await;
        self.state().pid = None;
        match r {
            Ok(s) => format!("exited ({s})"),
            Err(e) => format!("lost: {e}"),
        }
    }

    /// SIGTERM, then 10 s to end, then SIGKILL; not started again.
    pub async fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.stopped.notify_waiters();
        let pid = self.state().pid;
        if pid.is_some() {
            signal(pid, libc::SIGTERM);
            let t0 = Instant::now();
            while self.state().pid.is_some() && t0.elapsed() < Duration::from_secs(10) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if let Some(pid) = self.state().pid {
                tracing::warn!(app = %self.plan.name, pid, "did not stop in 10 s: killed");
                signal(Some(pid), libc::SIGKILL);
            }
        }
        // The supervisor, waiting to start it again or reaping it, ends.
        let t0 = Instant::now();
        while !self.done.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if pid.is_some() {
            tracing::info!(app = %self.plan.name, "stopped the app");
        }
    }
}

/// Sends `sig` to an app's process.
fn signal(pid: Option<u32>, sig: i32) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: a signal to a child this process started and has not
        // reaped (its pid is cleared once it is).
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role() -> Role {
        Role { replica: false, shard: false, beside_coordinator: false, kube: false, tls: false }
    }

    #[test]
    fn plans_the_apps_installed_beside_the_server() {
        let dir = std::env::temp_dir().join(format!("chloedb-app-procs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chloedb-sheets"), "").unwrap();
        std::fs::write(dir.join("chloectx"), "").unwrap();
        let p = plan("auto", &[], &role(), Some(&dir)).unwrap();
        assert_eq!(p.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["sheets", "chloectx"]);
        assert_ne!(p[0].token, p[1].token, "a token of its own each");
        assert_ne!(p[0].port, p[1].port);
        // Each port is held, so that no other process can bind it.
        for a in &p {
            assert_eq!(a.listener.as_ref().unwrap().local_addr().unwrap().port(), a.port);
            assert!(std::net::TcpListener::bind(("127.0.0.1", a.port)).is_err(), "port {} is the app's", a.port);
        }
        // An --app of the name wins.
        let p = plan("auto", &["sheets".into()], &role(), Some(&dir)).unwrap();
        assert_eq!(p.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["chloectx"]);
        assert!(plan("off", &[], &role(), Some(&dir)).unwrap().is_empty());
        assert!(plan("sheets", &["sheets".into()], &role(), Some(&dir)).is_err());
        assert!(plan("nope", &[], &role(), Some(&dir)).is_err());
        // A replica starts none, and says so when asked for one.
        let replica = Role { replica: true, ..role() };
        assert!(plan("auto", &[], &replica, Some(&dir)).unwrap().is_empty());
        assert!(plan("sheets", &[], &replica, Some(&dir)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_credential_of_the_server_reaches_an_app() {
        for k in ["CHLOEDB_TOKEN", "STRATA_TOKEN", "CHLOEDB_CLUSTER_TOKEN", "CHLOEDB_APP_TOKEN", "CHLOEDB_KEY", "CHLOECTX_APP_TOKEN", "CHLOEDB_SHEETS_TOKEN", "AWS_SECRET_ACCESS_KEY", "DIGITALOCEAN_TOKEN"] {
            assert!(!keep_env(k), "{k}");
        }
        for k in ["PATH", "HOME", "LC_ALL", "ANTHROPIC_API_KEY", "HTTPS_PROXY"] {
            assert!(keep_env(k), "{k}");
        }
    }

    #[test]
    fn each_app_gets_its_token_and_key_under_its_own_names() {
        let p = Plan { name: "chloectx".into(), bin: "chloectx".into(), port: 7761, listener: None, token: "t".into() };
        let (args, env) = command(&p, "http://127.0.0.1:7700", Some("sk_a_b"));
        assert_eq!(args, ["app", "--server", "http://127.0.0.1:7700", "--host", "127.0.0.1", "--port", "7761"]);
        assert!(env.contains(&("CHLOECTX_APP_TOKEN", "t".into())) && env.contains(&("CHLOEDB_KEY", "sk_a_b".into())) && env.contains(&("CHLOEDB_START_APPS", "off".into())));
        let p = Plan { name: "crawler".into(), ..p };
        let (args, env) = command(&p, "http://127.0.0.1:7700", None);
        assert!(args.windows(2).any(|w| w == ["--grow", "off"]));
        assert!(!env.iter().any(|(k, _)| *k == "CHLOEDB_KEY"));
        assert!(!env.iter().any(|(k, _)| *k == LISTEN_FD_ENV), "no socket to hand over");
        let (l, port) = listener().unwrap();
        let p = Plan { port, listener: Some(Arc::new(l)), ..p };
        assert!(command(&p, "http://127.0.0.1:7700", None).1.contains(&(LISTEN_FD_ENV, "3".into())));
    }
}
