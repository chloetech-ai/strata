//! Copies a project's memory to a ChloeDB server or cluster, and back.
//!
//! The store stays a local file, so that hooks and recall never wait on the
//! network. The daemon pushes the records it has not sent yet to its
//! project's table ([`table_for`], `chloectx_mem_<project>`) on the server
//! set up with `chloectx sync setup --url URL` every [`PUSH_EVERY`] and as
//! it exits, and when it starts it pulls the records of its project the
//! store lacks, which a store made on another machine (or a cloud
//! container started afresh) wrote, and embeds them here. A project is its
//! git repository (`owner/name`, see [`project_key`]), so every checkout
//! of it, under any folder or an old name, shares one memory.
//!
//! A table per project is the boundary: a key limited to one project's
//! table (`{"chloectx_mem_<project>": "write"}`, which the Inspector makes
//! per machine) reads and writes that project only. Before 0.3.4 every
//! project shared one table, [`TABLE`] (`chloectx_memory`), which any sync
//! key read all of; its rows of the project are still pulled where the
//! key may read them, and nothing is pushed there.
//!
//! Each row names the machine that pushed it (`origin`, [`origin`]). A
//! pulled record from another machine is linked `synced_from` it, and its
//! rules and decisions stay out of the briefing and the prompt's context
//! until a person accepts that machine (`chloectx sync accept`,
//! [`pending`]): a key that may write the table could otherwise plant a
//! "user preference" that every other machine shows as its own.
//!
//! What is sent is what the store holds: text from the sessions it
//! recorded, which can include whatever passed through them. It is
//! redacted again as it is sent and as it is pulled ([`crate::redact`]),
//! for stores written before redaction knew a secret's shape. The server
//! should take it over HTTPS and need a key (an API key with the `write`
//! scope is enough once the table exists; creating it needs `admin`).
//!
//! Records retagged or forgotten after they were pushed are not sent
//! again: the server keeps the version it was sent first. Records that
//! `chloectx redact` rewrites are ([`push_records`]).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use chloedb::query::Params;

use crate::record::{Kind, Link, Record, COLUMNS};
use crate::store::Store;
use crate::{err, Result};

/// How often the daemon pushes what it recorded since.
pub const PUSH_EVERY: Duration = Duration::from_secs(30);
/// Records per request.
const BATCH: usize = 500;
/// A request's deadline.
const TIMEOUT_SECS: u64 = 30;

/// The table every project shared before 0.3.4 (pulled from, where the
/// key reads it; never written).
pub const TABLE: &str = "chloectx_memory";
/// The prefix of the projects' tables ([`table_for`]).
pub const PREFIX: &str = "chloectx_mem_";

/// The table [`TABLE`], as `chloectx sync setup` created it.
pub const CREATE: &str = "CREATE TABLE IF NOT EXISTS chloectx_memory (id KEYWORD PRIMARY KEY, project KEYWORD, rid KEYWORD, session KEYWORD, kind KEYWORD, tag KEYWORD, turn INT, seq INT, ts INT, importance FLOAT, files TEXT, content TEXT, context TEXT, links_json TEXT)";
const REMOTE_COLUMNS: &str = "rid, session, kind, tag, turn, seq, ts, importance, files, content, context, links_json";
/// A project's table adds who pushed each row.
const PROJECT_COLUMNS: &str = "rid, session, kind, tag, turn, seq, ts, importance, files, content, context, links_json, origin";

/// The table of project `project` (its [`project_key`]): `chloectx_mem_`,
/// then the key in lowercase letters, digits and `_` (at most 40), then 8
/// hex digits of its hash, so that two keys that read the same there
/// (`acme/web-app`, `acme/web_app`) are two tables.
pub fn table_for(project: &str) -> String {
    let mut slug = String::new();
    for c in project.trim().to_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '_' };
        if c == '_' && (slug.is_empty() || slug.ends_with('_')) {
            continue;
        }
        slug.push(c);
    }
    let slug: String = slug.trim_end_matches('_').chars().take(40).collect();
    format!("{PREFIX}{}_{:08x}", slug.trim_end_matches('_'), fnv(project.trim()) as u32)
}

/// Project `project`'s table, as the setup (an admin's key) creates it.
pub fn create_for(project: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {} (id KEYWORD PRIMARY KEY, project KEYWORD, rid KEYWORD, session KEYWORD, kind KEYWORD, tag KEYWORD, turn INT, seq INT, ts INT, importance FLOAT, files TEXT, content TEXT, context TEXT, links_json TEXT, origin KEYWORD)",
        table_for(project)
    )
}

/// The label of the link a record pulled from another machine carries to
/// that machine's [`origin`], until it is accepted ([`ACCEPTED`]).
pub const SYNCED_FROM: &str = "synced_from";
/// The label once a person accepted the machine (`chloectx sync accept`).
pub const ACCEPTED: &str = "accepted_from";

/// This machine, as the rows it pushes name it: `$CHLOECTX_SYNC_ORIGIN`
/// (a cloud environment started afresh each time sets one), else a random
/// id kept in `sync-origin` in ChloeCtx's home, made on first use.
pub fn origin(home: &Path) -> String {
    if let Some(o) = std::env::var("CHLOECTX_SYNC_ORIGIN").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        return o;
    }
    let p = home.join("sync-origin");
    if let Some(o) = std::fs::read_to_string(&p).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        return o;
    }
    let mut b = [0u8; 6];
    let _ = getrandom(&mut b);
    let o = format!("m-{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
    let _ = crate::paths::create_private_dir(home);
    let _ = std::fs::write(&p, &o);
    o
}

fn getrandom(b: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(b)
}

/// Whether `r` came from another machine whose records were not accepted
/// yet: its rules and decisions stay out of the briefing, the prompt's
/// context and searches until then ([`held_back`]); `chloectx sync
/// accept` lists them.
pub fn pending(r: &Record) -> bool {
    r.links.iter().any(|l| l.label == SYNCED_FROM)
}

/// Whether `r` is a rule or decision from a machine not accepted yet,
/// which the briefing and the prompt's context leave out ([`pending`]).
pub fn held_back(r: &Record) -> bool {
    matches!(r.kind, Kind::Preference | Kind::Decision) && pending(r)
}

/// The machine a pending record came from.
pub fn pending_origin(r: &Record) -> Option<&str> {
    r.links.iter().find(|l| l.label == SYNCED_FROM).map(|l| l.to.as_str())
}

/// Where the sync is set up: `sync.json` (the URL) and `sync.token` (the
/// key, readable by its owner only) in ChloeCtx's home.
pub fn config_path(home: &Path) -> PathBuf {
    home.join("sync.json")
}

pub fn token_path(home: &Path) -> PathBuf {
    home.join("sync.token")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncConfig {
    pub url: String,
    /// The environment variable that holds the key (e.g. one a cloud
    /// environment sets), read when the daemon syncs: the key is kept in
    /// no file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token_env: String,
    /// Repositories renamed (`owner/old` to `owner/new`): a checkout of
    /// either name syncs as the new one.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub renames: std::collections::BTreeMap<String, String>,
    /// Machines ([`origin`]) whose records a pull takes as this one's own
    /// (`chloectx sync accept --origin`), `*` for every machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted: Vec<String>,
}

pub fn load_config(home: &Path) -> SyncConfig {
    std::fs::read_to_string(config_path(home)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// The repository `owner/name` a git remote URL names (https, ssh or a
/// proxy's `.../git/owner/name`), lowercased, without `.git`.
fn repo_of(url: &str) -> Option<String> {
    let path = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = path.rsplit_once(':').filter(|(h, _)| !h.contains("//")).map_or(path, |(_, p)| p);
    let mut parts = path.rsplit('/').filter(|p| !p.is_empty());
    let name = parts.next()?;
    let owner = parts.next()?;
    (!owner.contains('@') && !owner.contains(':')).then(|| format!("{owner}/{name}").to_lowercase())
}

/// What the project at `project` syncs as: its git repository
/// (`owner/name` of `origin`, renames applied), so that checkouts in
/// different folders, or of the repository under an old name, share their
/// memory; else its path.
pub fn project_key(home: &Path, project: &str) -> String {
    let cfg = load_config(home);
    let repo = Command::new("git").args(["-C", project, "remote", "get-url", "origin"]).stderr(Stdio::null()).output().ok().filter(|o| o.status.success()).and_then(|o| repo_of(&String::from_utf8_lossy(&o.stdout)));
    match repo {
        Some(r) => cfg.renames.get(&r).cloned().unwrap_or(r),
        None => project.to_string(),
    }
}

/// The server to sync with: `$CHLOECTX_SYNC_URL`, else `sync.json`; the
/// key: `$CHLOECTX_SYNC_TOKEN`, else the variable `sync.json` names, else
/// `sync.token` (none: the server is open, or a proxy adds it). `None` when
/// no URL is set.
pub fn remote(home: &Path) -> Option<Remote> {
    let cfg = load_config(home);
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = env("CHLOECTX_SYNC_URL").or_else(|| Some(cfg.url.clone()).filter(|u| !u.trim().is_empty()))?;
    let token = env("CHLOECTX_SYNC_TOKEN")
        .or_else(|| Some(cfg.token_env.as_str()).filter(|k| !k.is_empty()).and_then(env))
        .or_else(|| std::fs::read_to_string(token_path(home)).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Some(Remote { url: url.trim().trim_end_matches('/').to_string(), token, header_dir: home.to_path_buf() })
}

/// Refuses a server reached over plain HTTP anywhere but this machine:
/// what sync sends is transcripts of the user's sessions, and with them the
/// server's key. `CHLOECTX_SYNC_ALLOW_HTTP=1` allows it (a private network
/// behind a proxy the user trusts).
pub fn check_url(url: &str) -> Result<()> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    let Some(rest) = lower.strip_prefix("http://") else {
        return Err(err(format!("sync server {u}: give an https:// URL")));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = host.strip_prefix('[') { v6.split(']').next().unwrap_or("") } else { host.split(':').next().unwrap_or("") };
    if crate::app::loopback(host) || std::env::var("CHLOECTX_SYNC_ALLOW_HTTP").is_ok_and(|v| v == "1") {
        return Ok(());
    }
    Err(err(format!("sync server {u}: plain HTTP to another machine would send your sessions and the key in the clear; use https:// (or set CHLOECTX_SYNC_ALLOW_HTTP=1)")))
}

/// Writes the sync's settings (and the key, owner-only, when given).
pub fn setup(home: &Path, url: &str, token: Option<&str>, token_env: &str, renames: &[String]) -> Result<()> {
    check_url(url)?;
    crate::paths::create_private_dir(home)?;
    let mut map = std::collections::BTreeMap::new();
    for r in renames {
        let (old, new) = r.split_once('=').ok_or_else(|| err(format!("--rename {r}: give OLD=NEW, as owner/old=owner/new")))?;
        map.insert(old.trim().to_lowercase(), new.trim().to_lowercase());
    }
    let trusted = load_config(home).trusted;
    let cfg = SyncConfig { url: url.trim().to_string(), token_env: token_env.trim().to_string(), renames: map, trusted };
    std::fs::write(config_path(home), serde_json::to_string_pretty(&cfg).unwrap_or_default())?;
    if let Some(t) = token {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(token_path(home))?;
        f.write_all(t.trim().as_bytes())?;
    }
    Ok(())
}

/// A ChloeDB server's HTTP API, called with `curl`.
pub struct Remote {
    pub url: String,
    token: Option<String>,
    /// Where the request's header file is written (owner-only, removed
    /// after): the key is never on a command line.
    header_dir: PathBuf,
}

impl Remote {
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// POSTs `body` to `route`; the answer's JSON, or an error with the
    /// server's message.
    fn post(&self, route: &str, body: &Json) -> Result<Json> {
        if self.token.as_deref().is_some_and(is_anthropic_key) {
            return Err(err(format!("the sync's key is an Anthropic API key (sk-ant-...), not a ChloeDB token or API key (sk_<id>_<secret>): it was not sent to {}. Set the variable or sync.token to the server's key", self.url)));
        }
        check_url(&self.url)?;
        let mut cmd = Command::new("curl");
        cmd.args(["-sS", "-m", &TIMEOUT_SECS.to_string(), "-X", "POST", "-H", "content-type: application/json", "-H", "x-chloedb-client: chloectx-sync", "--data-binary", "@-", "-w", "\n%{http_code}"]);
        let header = match &self.token {
            Some(t) => {
                // A file of this request's own: a push and an explicit `sync`
                // may run at once, and neither may remove the other's.
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p = self.header_dir.join(format!(".sync-header-{}-{n}", std::process::id()));
                let _ = std::fs::remove_file(&p);
                let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p)?;
                writeln!(f, "Authorization: Bearer {t}")?;
                cmd.arg("-H").arg(format!("@{}", p.display()));
                Some(p)
            }
            None => None,
        };
        cmd.arg(format!("{}{route}", self.url));
        let out = (|| {
            let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| err(format!("curl: {e}")))?;
            child.stdin.take().expect("stdin").write_all(body.to_string().as_bytes())?;
            child.wait_with_output().map_err(|e| err(format!("curl: {e}")))
        })();
        if let Some(p) = header {
            let _ = std::fs::remove_file(p);
        }
        let out = out?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        let code: u16 = code.trim().parse().unwrap_or(0);
        if code == 0 {
            return Err(err(format!("{}: {}", self.url, String::from_utf8_lossy(&out.stderr).trim())));
        }
        let j: Json = serde_json::from_str(body).unwrap_or_else(|_| json!({"error": body.chars().take(300).collect::<String>()}));
        if !(200..300).contains(&code) {
            let msg = j.get("error").and_then(|e| e.as_str()).unwrap_or(body);
            return Err(err(format!("{}{route}: {code}: {msg}", self.url)));
        }
        Ok(j)
    }

    /// Creates project `project`'s table (once; needs the `admin` scope).
    pub fn create_table(&self, project: &str) -> Result<()> {
        self.post("/v1/sql", &json!({ "sql": create_for(project) })).map(|_| ())
    }

    fn rows(&self, sql: &str, params: Json) -> Result<Vec<Vec<Json>>> {
        let j = self.post("/v1/query", &json!({ "sql": sql, "params": params }))?;
        Ok(j.get("rows").and_then(|r| r.as_array()).map(|rs| rs.iter().filter_map(|r| r.as_array().cloned()).collect()).unwrap_or_default())
    }

    /// How many records of `project` the server holds.
    pub fn count(&self, project: &str) -> Result<i64> {
        let rows = self.rows(&format!("SELECT COUNT(*) FROM {} WHERE project = :p", table_for(project)), json!({ "p": project }))?;
        Ok(rows.first().and_then(|r| r.first()).and_then(|v| v.as_i64()).unwrap_or(0))
    }
}

/// What was pushed, per project store: `sync-state.json` beside the store.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    /// The server these marks are for: another URL starts again.
    pub url: String,
    /// Every record up to this sequence number was sent.
    pub pushed_seq: i64,
    pub last_push: i64,
    pub last_pull: i64,
    pub pushed: u64,
    pub pulled: u64,
    pub last_error: String,
}

fn state_path(store_dir: &Path) -> PathBuf {
    store_dir.join("sync-state.json")
}

pub fn load_state(store_dir: &Path, url: &str) -> SyncState {
    let s: SyncState = std::fs::read_to_string(state_path(store_dir)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if s.url == url {
        s
    } else {
        SyncState { url: url.to_string(), ..SyncState::default() }
    }
}

pub fn save_state(store_dir: &Path, s: &SyncState) {
    let tmp = state_path(store_dir).with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, state_path(store_dir));
    }
}

/// A record as the server's row, redacted, pushed by machine `origin`.
/// A record this store pulled keeps the machine it came from.
fn remote_doc(project: &str, origin: &str, r: &Record) -> Json {
    let mut r = r.clone();
    crate::redact::redact_record(&mut r);
    let origin = r.links.iter().find(|l| l.label == SYNCED_FROM || l.label == ACCEPTED).map_or(origin.to_string(), |l| l.to.clone());
    r.links.retain(|l| l.label != SYNCED_FROM && l.label != ACCEPTED);
    json!({
        "origin": origin,
        "id": format!("{project}:{}", r.id),
        "project": project,
        "rid": r.id,
        "session": r.session,
        "kind": r.kind.as_str(),
        "tag": r.tag,
        "turn": r.turn,
        "seq": r.seq,
        "ts": r.ts,
        "importance": r.importance,
        "files": r.files,
        "content": r.text,
        "context": r.ctx,
        "links_json": serde_json::to_string(&r.links).unwrap_or_else(|_| "[]".into()),
    })
}

/// A server's row (in [`REMOTE_COLUMNS`] order) as a record.
fn remote_record(row: &[Json]) -> Option<Record> {
    let s = |i: usize| row.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let n = |i: usize| row.get(i).and_then(|v| v.as_i64()).unwrap_or(0);
    let links: Vec<Link> = serde_json::from_str(&s(11)).unwrap_or_default();
    Some(Record {
        id: Some(s(0)).filter(|x| !x.is_empty())?,
        session: s(1),
        kind: Kind::parse(row.get(2)?.as_str()?)?,
        tag: s(3),
        turn: n(4),
        seq: n(5),
        ts: n(6),
        importance: row.get(7).and_then(|v| v.as_f64()).unwrap_or(0.0),
        files: s(8),
        text: s(9),
        ctx: s(10),
        links,
    })
}

/// Whether `e` says the server has no table `table`.
fn no_table(e: &crate::Error, table: &str) -> bool {
    e.to_string().contains(&format!("no such table: {table}"))
}

/// [`push_once`], creating the project's table first where the server
/// lacks it (a key with the `admin` scope can; the Inspector does it as it
/// makes a project's key).
pub fn push(remote: &Remote, store: &Store, project: &str, st: &mut SyncState) -> Result<usize> {
    match push_once(remote, store, project, st) {
        Err(e) if no_table(&e, &table_for(project)) => {
            remote.create_table(project).map_err(|c| {
                err(format!("{e}; creating it failed too ({c}): an admin makes it, and a key for it, with \"Make a sync key\" on the command center's ChloeCtx page, or with `chloectx sync setup` and an admin's key"))
            })?;
            push_once(remote, store, project, st)
        }
        r => r,
    }
}

/// Sends the records of `store` after `st.pushed_seq`; how many.
fn push_once(remote: &Remote, store: &Store, project: &str, st: &mut SyncState) -> Result<usize> {
    let table = table_for(project);
    let me = origin(&remote.header_dir);
    let mut sent = 0;
    loop {
        let rs = store.query_with(&format!("SELECT {COLUMNS} FROM memory WHERE seq > :s ORDER BY seq ASC LIMIT {BATCH}"), &Params::new().set("s", chloedb::Value::Int(st.pushed_seq)))?;
        let recs: Vec<Record> = rs.rows.iter().filter_map(|r| crate::store::row_record(r)).collect();
        let Some(top) = rs.rows.iter().filter_map(|r| r.get(5).and_then(|v| v.as_i64())).max() else { break };
        if !recs.is_empty() {
            let ops: Vec<Json> = recs.iter().map(|r| json!({"op": "upsert", "table": table, "doc": remote_doc(project, &me, r)})).collect();
            // Named by the records it holds: a retry of this batch is the
            // same request, and another machine's batch of the same
            // project and sequence numbers (each store numbers its own) is
            // not, which a key of the numbers alone said it was, so the
            // server refused it as a reused key for an hour.
            let ids = recs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(",");
            remote.post("/v1/write", &json!({ "ops": ops, "idempotency_key": format!("chloectx-{:016x}-{:016x}-{top}", fnv(project), fnv(&ids)) }))?;
            sent += recs.len();
        }
        st.pushed_seq = top;
        st.pushed += recs.len() as u64;
        st.last_push = crate::now();
        if rs.rows.len() < BATCH {
            break;
        }
    }
    Ok(sent)
}

/// Sends `recs` again, replacing the server's rows with the same ids (the
/// records `chloectx redact` rewrote); how many.
pub fn push_records(remote: &Remote, project: &str, recs: &[Record]) -> Result<usize> {
    let (table, me) = (table_for(project), origin(&remote.header_dir));
    for batch in recs.chunks(BATCH) {
        let ops: Vec<Json> = batch.iter().map(|r| json!({"op": "upsert", "table": table, "doc": remote_doc(project, &me, r)})).collect();
        let ids = batch.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(",");
        remote.post("/v1/write", &json!({ "ops": ops, "idempotency_key": format!("chloectx-redact-{:016x}-{:016x}-{}", fnv(project), fnv(&ids), crate::now()) }))?;
    }
    Ok(recs.len())
}

/// Stores the records of `project` the server holds and `store` lacks:
/// from the project's table, then from the table projects shared before
/// 0.3.4 ([`TABLE`]) where the key may read it. A server without a table
/// holds nothing to pull from it.
pub fn pull(remote: &Remote, store: &Store, project: &str, st: &mut SyncState) -> Result<usize> {
    let cfg = load_config(&remote.header_dir);
    let me = origin(&remote.header_dir);
    let table = table_for(project);
    let mut got = match pull_once(remote, store, project, &table, PROJECT_COLUMNS, &me, &cfg.trusted) {
        Err(e) if no_table(&e, &table) => 0,
        r => r?,
    };
    // The shared table may be gone, or not for this key (a project's key
    // is refused it): neither is an error of this project's sync.
    got += pull_once(remote, store, project, TABLE, REMOTE_COLUMNS, &me, &cfg.trusted).unwrap_or(0);
    st.pulled += got as u64;
    st.last_pull = crate::now();
    Ok(got)
}

/// The records of `project` in `table`, read as `cols`, that `store`
/// lacks, stored. Paged by sequence number: each machine numbers its own
/// records, so the server holds several of one number, and a full page
/// may end inside them, so its last number's records are read whole
/// before the next page starts after it.
fn pull_once(remote: &Remote, store: &Store, project: &str, table: &str, cols: &str, me: &str, trusted: &[String]) -> Result<usize> {
    let mut got = 0;
    let mut after = i64::MIN;
    loop {
        let mut rows = remote.rows(&format!("SELECT {cols} FROM {table} WHERE project = :p AND seq > :s ORDER BY seq ASC LIMIT {BATCH}"), json!({ "p": project, "s": after }))?;
        let full = rows.len() >= BATCH;
        let Some(top) = rows.iter().filter_map(|r| r.get(5).and_then(|v| v.as_i64())).max() else { break };
        if full {
            rows.retain(|r| r.get(5).and_then(|v| v.as_i64()) != Some(top));
            rows.extend(remote.rows(&format!("SELECT {cols} FROM {table} WHERE project = :p AND seq = :s LIMIT 10000"), json!({ "p": project, "s": top }))?);
        }
        got += store_new(store, &rows, me, trusted)?;
        after = top;
        if !full {
            break;
        }
    }
    Ok(got)
}

/// Stores the server's `rows` that `store` lacks, redacted and embedded
/// here; how many. A row another machine pushed (its `origin`, the
/// thirteenth column, is not `me`; a row of the shared table names none)
/// is linked [`SYNCED_FROM`] that machine, or [`ACCEPTED`] when it is in
/// `trusted`.
fn store_new(store: &Store, rows: &[Vec<Json>], me: &str, trusted: &[String]) -> Result<usize> {
    let mut recs: Vec<Record> = Vec::new();
    for row in rows {
        let Some(mut r) = remote_record(row) else { continue };
        crate::redact::redact_record(&mut r);
        let from = row.get(12).and_then(|v| v.as_str()).filter(|o| !o.is_empty()).unwrap_or("unknown");
        if from != me {
            let ok = trusted.iter().any(|t| t == "*" || t == from);
            r.links.retain(|l| l.label != SYNCED_FROM && l.label != ACCEPTED);
            r.links.push(Link { label: if ok { ACCEPTED } else { SYNCED_FROM }.into(), to: from.to_string() });
        }
        recs.push(r);
    }
    let ids: Vec<String> = recs.iter().map(|r| r.id.clone()).collect();
    let have: std::collections::HashSet<String> = store.get(&ids)?.into_iter().map(|r| r.id).collect();
    let new: Vec<Record> = recs.into_iter().filter(|r| !have.contains(&r.id)).collect();
    if new.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = new.iter().map(|r| r.embed_text()).collect();
    let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let embs = store.embedder.embed(&refs)?;
    store.bulk_insert(&new, embs)?;
    Ok(new.len())
}

/// `chloectx sync accept`: the store's records pulled from machines not
/// accepted yet, by machine (`origin`, count, a few of their texts); with
/// `origins` (`*` for all), those machines' records are accepted, and the
/// machines added to `sync.json`'s `trusted`, so that later pulls take
/// theirs as accepted too. Returns the machines and how many records each
/// had pending.
pub fn accept(home: &Path, store: &Store, origins: &[String]) -> Result<Vec<(String, usize, Vec<String>)>> {
    // Pulled records keep the server's sequence numbers: paged by them.
    let mut recs: Vec<Record> = Vec::new();
    let mut after = i64::MIN;
    loop {
        let rs = store.query_with(&format!("SELECT {COLUMNS} FROM memory WHERE seq > :s ORDER BY seq ASC LIMIT 5000"), &Params::new().set("s", chloedb::Value::Int(after)))?;
        let mut page: Vec<Record> = rs.rows.iter().filter_map(|r| crate::store::row_record(r)).collect();
        let Some(top) = page.iter().map(|r| r.seq).max() else { break };
        let full = page.len() >= 5000;
        if full {
            // Several machines' records share a number: the last one's whole.
            page.retain(|r| r.seq != top);
            let rest = store.query_with(&format!("SELECT {COLUMNS} FROM memory WHERE seq = :s LIMIT 100000"), &Params::new().set("s", chloedb::Value::Int(top)))?;
            page.extend(rest.rows.iter().filter_map(|r| crate::store::row_record(r)));
        }
        recs.extend(page.into_iter().filter(pending));
        if !full {
            break;
        }
        after = top;
    }
    let mut by: std::collections::BTreeMap<String, (usize, Vec<String>)> = Default::default();
    for r in &recs {
        let e = by.entry(pending_origin(r).unwrap_or("unknown").to_string()).or_default();
        e.0 += 1;
        if e.1.len() < 3 && r.kind.is_knowledge() {
            e.1.push(r.text.chars().take(120).collect());
        }
    }
    if !origins.is_empty() {
        let all = origins.iter().any(|o| o == "*");
        let mut changed = Vec::new();
        for mut r in recs {
            let from = pending_origin(&r).unwrap_or("unknown").to_string();
            if all || origins.contains(&from) {
                for l in r.links.iter_mut().filter(|l| l.label == SYNCED_FROM) {
                    l.label = ACCEPTED.into();
                }
                changed.push(r);
            }
        }
        if !changed.is_empty() {
            store.insert(&changed)?;
        }
        let mut cfg = load_config(home);
        for o in origins {
            if !cfg.trusted.contains(o) {
                cfg.trusted.push(o.clone());
            }
        }
        crate::paths::create_private_dir(home)?;
        std::fs::write(config_path(home), serde_json::to_string_pretty(&cfg).unwrap_or_default())?;
    }
    Ok(by.into_iter().map(|(o, (n, t))| (o, n, t)).collect())
}

/// Whether `t` is an Anthropic API key, which a ChloeDB server never takes:
/// a variable set to the wrong key would send it to the sync's server.
fn is_anthropic_key(t: &str) -> bool {
    t.trim_start().starts_with("sk-ant-")
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_names_its_repository() {
        for u in ["https://github.com/chloetech-ai/Strata", "https://github.com/chloetech-ai/strata.git", "git@github.com:chloetech-ai/strata.git", "http://local_proxy@127.0.0.1:1234/git/chloetech-ai/strata"] {
            assert_eq!(repo_of(u).as_deref(), Some("chloetech-ai/strata"), "{u}");
        }
        assert_eq!(repo_of("strata"), None);
    }

    #[test]
    fn each_project_has_a_table_of_its_own() {
        assert_eq!(table_for("chloetech-ai/chloedb"), format!("chloectx_mem_chloetech_ai_chloedb_{:08x}", fnv("chloetech-ai/chloedb") as u32));
        assert_ne!(table_for("acme/web-app"), table_for("acme/web_app"));
        let t = table_for("/home/someone/a folder/with a very long name that goes on and on and on");
        assert!(t.starts_with(PREFIX) && t.len() <= PREFIX.len() + 40 + 9, "{t}");
        assert!(t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'), "{t}");
        // Not a space's table (`ctx_*`), nor the shared one.
        assert!(!t.starts_with("ctx_") && !t.starts_with(TABLE));
        assert!(create_for("acme/web").contains(&table_for("acme/web")));
    }

    #[test]
    fn plain_http_is_for_this_machine_only() {
        for u in ["https://chloedb.example.com", "HTTPS://x", "http://127.0.0.1:7700", "http://localhost:7700/", "http://[::1]:7700", "http://user@127.0.0.1:1"] {
            assert!(check_url(u).is_ok(), "{u}");
        }
        std::env::remove_var("CHLOECTX_SYNC_ALLOW_HTTP");
        for u in ["http://chloedb.example.com", "http://10.0.0.5:7700", "http://127.0.0.1.example.com", "http://127.0.0.1@evil.example.com/", "ftp://x"] {
            assert!(check_url(u).is_err(), "{u}");
        }
        let r = Remote { url: "http://chloedb.example.com".into(), token: Some("sk_a_b".into()), header_dir: std::env::temp_dir() };
        let e = r.post("/v1/query", &json!({})).unwrap_err().to_string();
        assert!(e.contains("plain HTTP") && !e.contains("sk_a_b"), "{e}");
    }

    #[test]
    fn an_anthropic_key_is_never_sent() {
        let r = Remote { url: "http://127.0.0.1:9".into(), token: Some("sk-ant-api03-xxxx".into()), header_dir: std::env::temp_dir() };
        let e = r.post("/v1/query", &json!({})).unwrap_err().to_string();
        assert!(e.contains("Anthropic API key") && !e.contains("xxxx"), "{e}");
        assert!(!is_anthropic_key("sk_abc_def") && !is_anthropic_key("a-cluster-token"));
    }

    #[test]
    fn records_round_trip_through_the_servers_row() {
        let r = Record {
            id: "abcd1234:7".into(),
            session: "abcd1234-s".into(),
            kind: Kind::Decision,
            tag: "".into(),
            turn: 3,
            seq: 7,
            ts: 1_700_000_000,
            importance: 0.75,
            files: "src/a.rs".into(),
            text: "use ChloeDB".into(),
            ctx: "Turn 3".into(),
            links: vec![Link { label: "about".into(), to: "e:chloedb".into() }],
        };
        let d = remote_doc("/home/me/p", "m-1", &r);
        assert_eq!(d["origin"], "m-1");
        assert_eq!(d["id"], "/home/me/p:abcd1234:7");
        let row: Vec<Json> = REMOTE_COLUMNS.split(", ").map(|c| d[c].clone()).collect();
        let back = remote_record(&row).unwrap();
        assert_eq!(back.id, r.id);
        assert_eq!(back.kind, r.kind);
        assert_eq!(back.links, r.links);
        assert_eq!((back.seq, back.text.as_str(), back.ctx.as_str()), (7, "use ChloeDB", "Turn 3"));
    }

    #[test]
    fn what_is_sent_is_redacted_again() {
        // As a store written before redaction knew these shapes holds them.
        let pw = ["correct", "horse", "battery", "42"].join("-");
        let key = format!("sk_{}_{}", "9f8e7d6c5b4a3f2e", "0123456789abcdef".repeat(3));
        let r = Record {
            id: "abcd1234:8".into(),
            session: "abcd1234-s".into(),
            kind: Kind::ToolResult,
            tag: "Bash".into(),
            turn: 3,
            seq: 8,
            ts: 1_700_000_000,
            importance: 0.3,
            files: String::new(),
            text: format!("$ env\nDB_PASSWORD={pw}"),
            ctx: format!("Turn 3 request: use the key {key}"),
            links: Vec::new(),
        };
        let d = remote_doc("p", "m-1", &r).to_string();
        assert!(!d.contains(&pw) && !d.contains(&key), "{d}");
        assert!(d.contains("DB_PASSWORD=[REDACTED:password]") && d.contains("use the key [REDACTED:chloedb-key]"), "{d}");
    }
}
