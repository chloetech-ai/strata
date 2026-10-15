//! `engram ui`: the inspector (`demo/index.html`, built into the binary)
//! over the Engram stores on this machine.
//!
//! The page talks to this server; the server reads each store through its
//! project's daemon (starting it if needed), as the hooks and `engram rpc`
//! do, so the inspector can stay open while sessions write to a store.
//! Only read-only daemon operations are exposed. The server listens on
//! 127.0.0.1 by default and every request must carry the random token from
//! the URL it prints (kept in a cookie after the first page load): stores
//! hold transcripts, and other local users or web pages must not read them.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{json, Value as Json};

use crate::{daemon, paths, Result};

const PAGE: &str = include_str!("../demo/index.html");
/// Daemon ops the page may call (all read-only).
const RPC_OPS: [&str; 7] = ["inspect_query", "inspect_layout", "inspect_embed", "search", "timeline", "stats", "briefing"];
const MAX_BODY: usize = 1 << 20;

pub struct Options {
    pub host: String,
    pub port: u16,
    /// Directory holding `projects/<store>/` (default: [`paths::engram_home`]).
    pub home: PathBuf,
    /// Open a browser on the printed URL.
    pub open: bool,
    /// The project whose store is shown first (default: the current directory's).
    pub project: PathBuf,
}

/// One store directory under `<home>/projects`.
#[derive(Clone, Debug)]
pub struct StoreInfo {
    pub id: String,
    pub dir: PathBuf,
    /// The project directory the store records, when known.
    pub project: Option<String>,
    pub bytes: u64,
    pub modified: i64,
}

impl StoreInfo {
    fn json(&self) -> Json {
        let running = std::os::unix::net::UnixStream::connect(paths::StorePaths::new(&self.dir).socket).is_ok();
        json!({
            "id": self.id, "dir": self.dir, "project": self.project, "bytes": self.bytes, "modified": self.modified, "running": running,
            "project_exists": self.project.as_deref().is_some_and(|p| Path::new(p).is_dir()),
        })
    }
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64)
}

/// The project a store belongs to: `project.txt` (written by the daemon),
/// else the last "serving <project> on <socket>" line of `daemon.log`
/// (stores from before `project.txt` existed).
pub fn store_project(dir: &Path) -> Option<String> {
    if let Ok(p) = std::fs::read_to_string(dir.join("project.txt")) {
        let p = p.trim();
        if !p.is_empty() {
            return Some(p.to_string());
        }
    }
    let log = std::fs::read_to_string(dir.join("daemon.log")).ok()?;
    log.lines().rev().find_map(|l| {
        let rest = l.strip_prefix("engram: serving ")?;
        let before = &rest[..rest.rfind(".sock (")?];
        Some(before[..before.rfind(" on ")?].to_string())
    })
}

/// Every store under `<home>/projects`, most recently written first.
pub fn list_stores(home: &Path) -> Vec<StoreInfo> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(home.join("projects")) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        let db = dir.join("memory.strata");
        if !db.is_file() {
            continue;
        }
        let wal = dir.join("memory.strata-wal");
        let bytes = [&db, &wal].iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        let modified = [db.clone(), wal, dir.join("events.log"), dir.join("state.json")].iter().map(|p| mtime(p)).max().unwrap_or(0);
        out.push(StoreInfo { id: e.file_name().to_string_lossy().into_owned(), project: store_project(&dir), dir, bytes, modified });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.id.cmp(&b.id)));
    out
}

/// Sessions the daemon keeps state for, with when each was last seen and
/// its turn count, most recent first.
fn sessions(dir: &Path) -> Json {
    let state: Json = std::fs::read_to_string(dir.join("state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null);
    let mut v: Vec<Json> = state
        .get("sessions")
        .and_then(|s| s.as_object())
        .map(|m| m.iter().map(|(id, s)| json!({"id": id, "last_seen": s.get("last_seen").cloned().unwrap_or(json!(0)), "turns": s.get("turn").cloned().unwrap_or(json!(0))})).collect())
        .unwrap_or_default();
    v.sort_by_key(|s| std::cmp::Reverse(s["last_seen"].as_i64().unwrap_or(0)));
    Json::Array(v)
}

/// Lines of `events.log` from byte `offset` (whole lines only), and the
/// offset after them.
fn read_events(dir: &Path, offset: u64) -> (Vec<Json>, u64) {
    let path = dir.join("events.log");
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let offset = if offset > size { 0 } else { offset };
    if size == offset {
        return (Vec::new(), offset);
    }
    let mut buf = Vec::new();
    if let Ok(mut f) = std::fs::File::open(&path) {
        use std::io::Seek;
        let _ = f.seek(std::io::SeekFrom::Start(offset));
        let _ = f.take(size - offset).read_to_end(&mut buf);
    }
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let events = buf[..end].split(|&b| b == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect();
    (events, offset + end as u64)
}

struct Server {
    opts: Options,
    token: String,
    cookie: String,
    default_store: Option<String>,
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        if headers.len() > 100 {
            return None;
        }
    }
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return None;
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let query = qs.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (percent_decode(k), percent_decode(v))).collect();
    Some(Request { method, path: path.to_string(), query, headers, body })
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, extra: &[(&str, String)], body: &[u8]) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond_json(stream: &mut TcpStream, status: &str, v: &Json) {
    respond(stream, status, "application/json", &[], v.to_string().as_bytes());
}

/// Compares two strings in time independent of where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Server {
    fn store(&self, req: &Request) -> Option<StoreInfo> {
        let stores = list_stores(&self.opts.home);
        let id = req.param("store").filter(|s| !s.is_empty()).map(str::to_string).or_else(|| self.default_store.clone());
        match id {
            Some(id) => stores.into_iter().find(|s| s.id == id),
            None => stores.into_iter().next(),
        }
    }

    fn rpc(&self, s: &StoreInfo, req: &Json, secs: u64) -> Json {
        match daemon::request(&s.dir, s.project.as_deref().unwrap_or(""), req, Duration::from_secs(secs)) {
            Ok(r) => r,
            Err(e) => json!({"error": e.to_string()}),
        }
    }

    fn authorized(&self, req: &Request) -> bool {
        if req.param("token").is_some_and(|t| same(t, &self.token)) {
            return true;
        }
        req.header("cookie").is_some_and(|c| {
            c.split(';').filter_map(|kv| kv.trim().split_once('=')).any(|(k, v)| k == self.cookie && same(v, &self.token))
        })
    }

    fn handle(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let Some(req) = read_request(&stream) else {
            respond(&mut stream, "400 Bad Request", "text/plain", &[], b"bad request");
            return;
        };
        if !self.authorized(&req) {
            let msg = "Open the URL that `engram ui` printed (it carries the access token).\n";
            respond(&mut stream, "403 Forbidden", "text/plain; charset=utf-8", &[], msg.as_bytes());
            return;
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => {
                if req.param("token").is_some() {
                    // Keep the token in a cookie and out of the address bar.
                    let rest: Vec<String> = req.query.iter().filter(|(k, _)| k != "token").map(|(k, v)| format!("{k}={}", urlencode(v))).collect();
                    let loc = if rest.is_empty() { "/".to_string() } else { format!("/?{}", rest.join("&")) };
                    let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Strict", self.cookie, self.token);
                    respond(&mut stream, "303 See Other", "text/plain", &[("Set-Cookie", cookie), ("Location", loc)], b"");
                } else {
                    // The page starts in browse mode rather than the demo's chat.
                    let page = PAGE.replacen("<body>", "<body class=\"browse\">", 1);
                    respond(&mut stream, "200 OK", "text/html; charset=utf-8", &[], page.as_bytes());
                }
            }
            ("GET", "/api/stores") => {
                let stores: Vec<Json> = list_stores(&self.opts.home).iter().map(StoreInfo::json).collect();
                respond_json(&mut stream, "200 OK", &json!({"home": self.opts.home, "stores": stores}));
            }
            ("GET", "/api/info") => {
                let stores: Vec<Json> = list_stores(&self.opts.home).iter().map(StoreInfo::json).collect();
                let mut info = json!({"mode": "browse", "home": self.opts.home, "stores": stores, "sessions": [], "current": null, "busy": false});
                if let Some(s) = self.store(&req) {
                    info["store_id"] = json!(s.id);
                    info["store"] = json!(s.dir.join("memory.strata"));
                    info["project"] = json!(s.project);
                    info["sessions"] = sessions(&s.dir);
                    info["events_bytes"] = json!(std::fs::metadata(s.dir.join("events.log")).map(|m| m.len()).unwrap_or(0));
                    info["stats"] = self.rpc(&s, &json!({"op": "stats"}), 60);
                }
                respond_json(&mut stream, "200 OK", &info);
            }
            ("GET", "/api/live") => {
                let Some(s) = self.store(&req) else { return respond_json(&mut stream, "404 Not Found", &json!({"error": "no such store"})) };
                let seq: i64 = req.param("seq").and_then(|v| v.parse().ok()).unwrap_or(0);
                let log: u64 = req.param("log").and_then(|v| v.parse().ok()).unwrap_or(0);
                let records = self.rpc(
                    &s,
                    &json!({"op": "inspect_query", "sql": format!("SELECT id, session, kind, tag, turn, seq, ts, importance, files, text, links FROM memory WHERE seq > {seq} ORDER BY seq ASC LIMIT 500")}),
                    60,
                );
                let (events, log_offset) = read_events(&s.dir, log);
                respond_json(&mut stream, "200 OK", &json!({"records": records, "events": events, "log_offset": log_offset, "busy": false}));
            }
            ("GET", "/api/events") => {
                let Some(s) = self.store(&req) else { return respond_json(&mut stream, "404 Not Found", &json!({"error": "no such store"})) };
                let session = req.param("session").unwrap_or("");
                let (events, _) = read_events(&s.dir, 0);
                let mut ev: Vec<Json> = events.into_iter().filter(|e| e.get("session").and_then(|v| v.as_str()) == Some(session)).collect();
                if ev.len() > 5000 {
                    ev.drain(..ev.len() - 5000);
                }
                respond_json(&mut stream, "200 OK", &json!({"events": ev}));
            }
            ("POST", "/api/rpc") => {
                let Some(s) = self.store(&req) else { return respond_json(&mut stream, "404 Not Found", &json!({"error": "no such store"})) };
                let body: Json = serde_json::from_slice(&req.body).unwrap_or(Json::Null);
                let op = body.get("op").and_then(|v| v.as_str()).unwrap_or("");
                if !RPC_OPS.contains(&op) {
                    return respond_json(&mut stream, "400 Bad Request", &json!({"error": format!("op {op:?} is not allowed here")}));
                }
                respond_json(&mut stream, "200 OK", &self.rpc(&s, &body, 120));
            }
            ("POST", "/api/chat") | ("POST", "/api/session") | ("POST", "/api/stop") => {
                respond_json(&mut stream, "404 Not Found", &json!({"error": "chat runs only in the demo (demo/run.sh); engram ui only reads stores"}))
            }
            _ => respond_json(&mut stream, "404 Not Found", &json!({"error": "not found"})),
        }
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let t = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b.copy_from_slice(&(t ^ (std::process::id() as u128) << 64).to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(cmd).arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

/// Runs the inspector server until interrupted.
pub fn run(opts: Options) -> Result<()> {
    // A daemon this server starts serves the hooks too until it idles out,
    // so give it the distill model the user-level install set.
    if std::env::var_os("ENGRAM_DISTILL_MODEL").is_none() {
        let settings: Json = std::fs::read_to_string(paths::user_settings()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null);
        if let Some(m) = settings.pointer("/env/ENGRAM_DISTILL_MODEL").and_then(|v| v.as_str()) {
            std::env::set_var("ENGRAM_DISTILL_MODEL", m);
        }
    }
    let mut listener = None;
    let tries = if opts.port == 0 { 1 } else { 10 };
    for i in 0..tries {
        match TcpListener::bind((opts.host.as_str(), opts.port.saturating_add(i))) {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && i + 1 < tries => continue,
            Err(e) => return Err(crate::err(format!("cannot listen on {}:{}: {e}", opts.host, opts.port.saturating_add(i)))),
        }
    }
    let listener = listener.expect("bound");
    let port = listener.local_addr()?.port();
    let token = random_token();
    let stores = list_stores(&opts.home);
    let own = paths::store_dir(&opts.project);
    let default_store = stores.iter().find(|s| s.dir == own).or(stores.first()).map(|s| s.id.clone());
    let shown_host = if opts.host == "0.0.0.0" || opts.host == "::" { "127.0.0.1".to_string() } else { opts.host.clone() };
    let shown_host = if shown_host.contains(':') { format!("[{shown_host}]") } else { shown_host };
    let url = format!("http://{shown_host}:{port}/?token={token}");
    eprintln!("engram ui: {} store{} in {}", stores.len(), if stores.len() == 1 { "" } else { "s" }, opts.home.join("projects").display());
    if !is_loopback(&opts.host) {
        eprintln!("engram ui: listening on {}:{port}; anyone who can reach it and has the token can read every store", opts.host);
    }
    eprintln!("engram ui: open {url}");
    if opts.open && std::env::var_os("SSH_CONNECTION").is_none() {
        open_browser(&url);
    }
    let server = std::sync::Arc::new(Server { cookie: format!("engram_ui_{port}"), token, default_store, opts });
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let s = server.clone();
        std::thread::spawn(move || s.handle(stream));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_from_log_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(store_project(d), None);
        std::fs::write(
            d.join("daemon.log"),
            "engram: serving /a b/on x on /s/engram.sock (0 records, embedder static)\nengram: idle, exiting\nengram: serving /work/proj on /tmp/engram-501/engram-00ff.sock (12 records, embedder hash:64)\n",
        )
        .unwrap();
        assert_eq!(store_project(d).as_deref(), Some("/work/proj"));
        std::fs::write(d.join("project.txt"), "/work/other\n").unwrap();
        assert_eq!(store_project(d).as_deref(), Some("/work/other"));
    }

    #[test]
    fn lists_stores_and_reads_events() {
        let home = tempfile::tempdir().unwrap();
        let a = home.path().join("projects/a-0001");
        let b = home.path().join("projects/b-0002");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(home.path().join("projects/empty")).unwrap();
        std::fs::write(a.join("memory.strata"), [0u8; 10]).unwrap();
        std::fs::write(b.join("memory.strata"), [0u8; 20]).unwrap();
        std::fs::write(b.join("events.log"), "{\"event\":\"recall\",\"session\":\"s1\"}\n{\"event\":\"distill\",\"session\":\"s2\"}\n{\"partial\":").unwrap();
        let ids: Vec<String> = list_stores(home.path()).into_iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"a-0001".to_string()) && ids.contains(&"b-0002".to_string()));
        let (ev, off) = read_events(&b, 0);
        assert_eq!(ev.len(), 2);
        let (more, off2) = read_events(&b, off);
        assert!(more.is_empty());
        assert_eq!(off, off2);
    }

    #[test]
    fn query_strings_decode() {
        assert_eq!(percent_decode("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
