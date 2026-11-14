//! Connectors: other systems' data into tables, configured as rows of a
//! table, each with a key of its own.
//!
//! A connector reads a system on a schedule (a pull: Prometheus, Jira,
//! ServiceNow, Oxidized, any REST API that answers JSON) or takes what a
//! system sends (a push: a webhook, Alertmanager, an email, Oxidized's
//! hook), turns each record into a row of its target table by a map
//! ([`map`]), and upserts the rows by the table's key, so a record read
//! twice is the same row.
//!
//! - **Configured as rows.** Each connector is a row of
//!   `chloedb_connectors` (name, kind, target, schedule, its config as
//!   JSON), which the API writes and SQL reads; what each run did is a row
//!   of `chloedb_connector_runs` (kept 30 days), and each connector's
//!   cursor, counts and last error a row of `chloedb_connector_state`.
//!   The tables are made with the first connector, and they are the
//!   server's own: no key writes them but the connectors' own
//!   ([`Connectors::may_write`]), so a config is changed by an admin
//!   through `/v1/admin/connectors`, checked there, and checked again each
//!   time the schedule reads it ([`Connector::check`]). Secrets
//!   (passwords, tokens) are never in them: they are kept in
//!   `<db>.connectors.json` beside the database, readable by the server's
//!   user only (`--connector-secrets FILE`; a coordinator without one
//!   keeps them in memory), or named as `env:VARIABLE`, a variable whose
//!   name starts with the prefix the operator chose
//!   (`CHLOEDB_CONNECTOR_SECRET_` by default, `--connector-env-prefix`).
//! - **What it reaches.** The public internet only, unless its config
//!   says `"allow_internal": true`, and never a link-local address (cloud
//!   metadata); its credentials go to its configured origin only, and an
//!   answer is read up to a cap ([`reach`], [`kinds`]).
//! - **A key each.** A run writes through the server's own API (in the
//!   process, as `/v1/mcp`'s tools do: [`Connectors::call`]) with a key
//!   made for it, limited to writing its target table; the connectors'
//!   own tables are written with another, limited to those. A push
//!   connector gets a key of the keys file, named `connector:<name>`,
//!   limited to writing its target, which the sending system holds: a
//!   push is written with the sender's credential, so the key that one
//!   leaks cannot read or write anything else. It rotates and is revoked
//!   in Access as any key.
//! - **Who runs the pulls.** A server of its own, or a coordinator (with
//!   several, the one that sends the alerts' messages: [`crate::alerts`]);
//!   never a replica or a shard (`--connectors off` turns them off). A
//!   push goes to any coordinator.
//! - **Health.** `GET /v1/admin/connectors` answers each connector's
//!   health (ok, failing, stale, waiting, pending, disabled), its last
//!   runs and its counts; the command center's Connectors page shows it.

pub mod kinds;
pub mod mail;
pub mod map;
pub mod reach;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request};
use axum::Router;
use serde::Serialize;
use serde_json::{json, Map, Value as Json};

use crate::keys::{Grant, Scope};
use crate::service::Service;
use crate::telemetry::now_ms;
use crate::ApiError;

/// The table of connectors' configs.
pub const CONFIGS: &str = "chloedb_connectors";
/// The table of their runs.
pub const RUNS: &str = "chloedb_connector_runs";
/// The table of their cursors and counts.
pub const STATE: &str = "chloedb_connector_state";

const SCHEMAS: &[(&str, &str)] = &[
    (CONFIGS, "CREATE TABLE IF NOT EXISTS chloedb_connectors (id KEYWORD PRIMARY KEY, kind KEYWORD, target KEYWORD, enabled BOOL, every_s INT, config TEXT OPTIONS(indexed = false), push_key KEYWORD, note TEXT, created TIMESTAMP, updated TIMESTAMP, updated_by KEYWORD)"),
    (RUNS, "CREATE TABLE IF NOT EXISTS chloedb_connector_runs (id KEYWORD PRIMARY KEY, connector KEYWORD, kind KEYWORD, target KEYWORD, trigger KEYWORD, started TIMESTAMP, ended TIMESTAMP, took_ms INT, ok BOOL, records INT, rows_ INT, skipped INT, dropped KEYWORD[], error TEXT, cursor TEXT OPTIONS(indexed = false), more BOOL) OPTIONS(retain = '30 days', retain_by = 'started')"),
    (STATE, "CREATE TABLE IF NOT EXISTS chloedb_connector_state (id KEYWORD PRIMARY KEY, cursor TEXT OPTIONS(indexed = false), last_run TIMESTAMP, last_ok TIMESTAMP, last_error TEXT, failures INT, runs INT, records_total INT, rows_total INT, last_rows INT, more BOOL)"),
];

/// `rows` in writes of [`BATCH`] rows or about 4 MB at most.
fn batches(rows: &[Map<String, Json>]) -> Vec<&[Map<String, Json>]> {
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0, 0usize);
    for (i, r) in rows.iter().enumerate() {
        let n = r.values().map(|v| match v {
            Json::String(s) => s.len() + 8,
            v => v.to_string().len() + 8,
        }).sum::<usize>();
        if i > start && (i - start >= BATCH || bytes + n > 4 << 20) {
            out.push(&rows[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += n;
    }
    if start < rows.len() {
        out.push(&rows[start..]);
    }
    out
}

/// The most records one run reads.
const RUN_LIMIT: usize = 20_000;
/// The most bytes of a push's body ([`push`]): smaller than an import's.
pub const PUSH_BODY_MAX: usize = 32 << 20;
/// The words a push of too many records is refused with (413).
const TOO_MANY: &str = "a push takes";
/// The prefix an `env:` secret's variable has unless the operator chose
/// another (`--connector-env-prefix`).
pub const ENV_PREFIX: &str = "CHLOEDB_CONNECTOR_SECRET_";

tokio::task_local! {
    /// Whether the request being served is the connectors' own, made in
    /// this process ([`Connectors::own_sql`], [`Connectors::own_write`]):
    /// the routes run in the task that makes it, so no request from
    /// outside is in it.
    static OWN_WRITE: bool;
}

/// Refuses (403) a change of `table` past the connectors' API, unless it
/// is not one of the connectors' own tables (`chloedb_connector*`), or the
/// request is the connectors' own ([`OWN_WRITE`]) or the server's root
/// credential's ([`crate::service::own`]: a coordinator's call to its
/// shards, the operator's token). Every key is refused, an admin's too: a
/// config is changed through `/v1/admin/connectors`, which checks it
/// ([`spec`]), keeps its secrets out of it and writes down who changed it,
/// where a row a key could write would pick the URL a connector sends its
/// credentials to and the table its run's key writes (audit 4, X-N1).
/// Checked on every write ([`crate::service::Service::write_ops`]) and
/// every statement ([`crate::service::Service::sql_in`]).
pub fn may_change(table: &str, what: &str) -> Result<(), ApiError> {
    if !own_table(table) || crate::service::own() || OWN_WRITE.try_with(|o| *o).unwrap_or(false) {
        return Ok(());
    }
    Err(ApiError {
        code: Some("server_owned"),
        status: 403,
        message: format!("{what} of {table} is refused: it is one of the connectors' own tables, which only the server writes; change a connector with POST /v1/admin/connectors/NAME and delete one with DELETE /v1/admin/connectors/NAME (an admin's)"),
    })
}

/// [`may_change`] for each of write operations `ops`, `default` naming
/// the table of one that names none.
pub fn may_write_ops(ops: &[Json], default: impl Fn() -> Option<String>) -> Result<(), ApiError> {
    if crate::service::own() || OWN_WRITE.try_with(|o| *o).unwrap_or(false) {
        return Ok(());
    }
    let mut default_checked = false;
    for op in ops {
        match op["table"].as_str() {
            Some(t) => may_change(t, "a write")?,
            None if !default_checked => {
                default_checked = true;
                if let Some(t) = default() {
                    may_change(&t, "a write")?;
                }
            }
            None => {}
        }
    }
    Ok(())
}

/// Whether `table` is one of the connectors' own tables
/// (`chloedb_connector*`), which only the server writes.
pub fn own_table(table: &str) -> bool {
    table.len() >= 17 && table[..17].eq_ignore_ascii_case("chloedb_connector")
}
/// Operations in one write.
const BATCH: usize = 500;

/// One connector: a row of [`CONFIGS`].
#[derive(Clone, Debug, Serialize)]
pub struct Connector {
    pub name: String,
    pub kind: String,
    pub target: String,
    pub enabled: bool,
    /// Seconds between pulls (0: pushed only).
    pub every_s: u64,
    pub config: Json,
    /// The id of its push key, if it has one.
    pub push_key: Option<String>,
    pub note: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Who changed it last (a key's name, or "token").
    pub updated_by: String,
    /// Why the row cannot run, when it fails [`Connector::check`] (a row
    /// written past the API): it is shown, and neither pulled nor pushed
    /// to, until an admin changes or deletes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
}

impl Connector {
    fn from_row(r: &Json) -> Option<Connector> {
        let name = r["id"].as_str()?.to_string();
        let ms = |k: &str| r[k].as_str().and_then(map::time_text).map_or(0, |us| (us / 1000) as u64);
        Some(Connector {
            name,
            kind: r["kind"].as_str().unwrap_or_default().to_string(),
            target: r["target"].as_str().unwrap_or_default().to_string(),
            enabled: r["enabled"].as_bool().or_else(|| r["enabled"].as_i64().map(|x| x != 0)).unwrap_or(true),
            every_s: r["every_s"].as_u64().unwrap_or(0),
            config: r["config"].as_str().and_then(|c| serde_json::from_str(c).ok()).unwrap_or_else(|| json!({})),
            push_key: r["push_key"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
            note: r["note"].as_str().unwrap_or_default().to_string(),
            created_ms: ms("created"),
            updated_ms: ms("updated"),
            updated_by: r["updated_by"].as_str().unwrap_or_default().to_string(),
            invalid: None,
        })
        .map(|mut c| {
            c.invalid = c.check().err();
            c
        })
    }

    /// What a connector must be to run, checked when the API is given one
    /// ([`spec`]) and again on every row the schedule, a run or a push
    /// reads: a name, a kind, a target that is a table's name (no grant
    /// pattern, not the connectors' own), a schedule of 10 s or more, a
    /// config of 64 KiB at most whose map compiles.
    pub fn check(&self) -> Result<(), String> {
        if !name_ok(&self.name) {
            return Err(format!("{:?} is not a connector's name: 1 to 48 lowercase letters, digits, - and _", self.name));
        }
        let k = kinds::kind(&self.kind).ok_or_else(|| format!("{:?} is not a kind of connector: {}", self.kind, kinds::KINDS.iter().map(|k| k.name).collect::<Vec<_>>().join(", ")))?;
        if !table_ok(&self.target) || own_table(&self.target) {
            return Err(format!("{:?} is not a table a connector may write: letters, digits and _, and not the connectors' own", self.target));
        }
        if k.pull && self.every_s > 0 && self.every_s < 10 {
            return Err("every_s is 10 seconds at least (or 0, run by hand only)".into());
        }
        if !self.config.is_object() {
            return Err("config is an object".into());
        }
        if self.config.to_string().len() > 64 << 10 {
            return Err("a connector's config is 64 KiB at most".into());
        }
        if let Some(Json::Object(m)) = self.config.get("map") {
            for (col, spec) in m {
                map::eval(&json!({}), spec, 0).map_err(|e| format!("map column {col}: {e}"))?;
            }
        }
        kinds::check_config(self)
    }

    fn row(&self) -> Json {
        json!({
            "id": self.name, "kind": self.kind, "target": self.target, "enabled": self.enabled, "every_s": self.every_s,
            "config": self.config.to_string(), "push_key": self.push_key.clone().unwrap_or_default(), "note": self.note,
            "created": chloedb::timestamp::format(self.created_ms as i64 * 1000), "updated": chloedb::timestamp::format(self.updated_ms as i64 * 1000),
            "updated_by": self.updated_by,
        })
    }

    fn pulls(&self) -> bool {
        kinds::kind(&self.kind).is_some_and(|k| k.pull) && self.every_s > 0
    }

    fn pushed(&self) -> bool {
        kinds::kind(&self.kind).is_some_and(|k| k.push)
    }
}

/// A connector's cursor and counts: a row of [`STATE`].
#[derive(Clone, Debug, Default, Serialize)]
pub struct State {
    pub cursor: Option<String>,
    pub last_run_ms: u64,
    pub last_ok_ms: u64,
    pub last_error: Option<String>,
    /// Failed runs since the last that went well.
    pub failures: u64,
    pub runs: u64,
    pub records_total: u64,
    pub rows_total: u64,
    pub last_rows: u64,
    /// The last run stopped at its limit.
    pub more: bool,
}

impl State {
    fn from_row(r: &Json) -> State {
        let ms = |k: &str| r[k].as_str().and_then(map::time_text).map_or(0, |us| (us / 1000) as u64);
        let n = |k: &str| r[k].as_u64().unwrap_or(0);
        State {
            cursor: r["cursor"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
            last_run_ms: ms("last_run"),
            last_ok_ms: ms("last_ok"),
            last_error: r["last_error"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
            failures: n("failures"),
            runs: n("runs"),
            records_total: n("records_total"),
            rows_total: n("rows_total"),
            last_rows: n("last_rows"),
            more: r["more"].as_bool().or_else(|| r["more"].as_i64().map(|x| x != 0)).unwrap_or(false),
        }
    }

    fn row(&self, name: &str) -> Json {
        let t = |ms: u64| if ms == 0 { Json::Null } else { json!(chloedb::timestamp::format(ms as i64 * 1000)) };
        json!({
            "id": name, "cursor": self.cursor.clone().unwrap_or_default(), "last_run": t(self.last_run_ms), "last_ok": t(self.last_ok_ms),
            "last_error": self.last_error.clone().unwrap_or_default(), "failures": self.failures, "runs": self.runs,
            "records_total": self.records_total, "rows_total": self.rows_total, "last_rows": self.last_rows, "more": self.more,
        })
    }
}

/// The connectors' secrets: by connector, by name.
#[derive(Default)]
struct Secrets {
    file: Option<PathBuf>,
    map: BTreeMap<String, BTreeMap<String, String>>,
}

/// What started a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    Manual,
    Push,
    Test,
}

impl Trigger {
    fn name(self) -> &'static str {
        match self {
            Trigger::Schedule => "schedule",
            Trigger::Manual => "manual",
            Trigger::Push => "push",
            Trigger::Test => "test",
        }
    }
}

/// A server's connectors ([`self`]).
#[derive(Default)]
pub struct Connectors {
    /// Pulls run here on their schedules (set by `main` for a server of
    /// its own or a coordinator).
    enabled: AtomicBool,
    started: AtomicBool,
    /// The API's routes, which the runs write through, and the host they
    /// name (the first `--allow-host`, if any).
    router: OnceLock<Router>,
    host: OnceLock<String>,
    tables_made: AtomicBool,
    /// The connectors read last, and when.
    cache: Mutex<Option<(u64, Vec<Connector>)>>,
    state: Mutex<HashMap<String, State>>,
    state_loaded: AtomicBool,
    running: Mutex<HashSet<String>>,
    secrets: Mutex<Secrets>,
    /// The key the connectors' own tables are written with.
    own_key: Mutex<Option<String>>,
    /// The prefix of the variables `env:` secrets may name (none set:
    /// [`ENV_PREFIX`]).
    env_prefix: OnceLock<String>,
    wake: tokio::sync::Notify,
    /// The clients of connectors that reach the public internet only, and
    /// of those that may reach this network too ([`reach`]).
    http: OnceLock<reqwest::Client>,
    http_internal: OnceLock<reqwest::Client>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 48 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn table_ok(t: &str) -> bool {
    !t.is_empty() && t.len() <= 128 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !t.starts_with('_')
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::bad(m)
}

impl Connectors {
    /// Pulls run on this server's schedules from now on.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Keeps the secrets in `file` (read now, if it exists).
    pub fn keep_secrets(&self, file: PathBuf) -> Result<(), String> {
        let map = match std::fs::read(&file) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", file.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        *lock(&self.secrets) = Secrets { file: Some(file), map };
        Ok(())
    }

    /// The secrets file of a database: `<db>.connectors.json`.
    pub fn file_of(db: &std::path::Path) -> PathBuf {
        let mut p = db.as_os_str().to_owned();
        p.push(".connectors.json");
        p.into()
    }

    /// The client of connector `c`, by what it may reach ([`reach`]).
    fn http(&self, c: &Connector) -> &reqwest::Client {
        match kinds::internal(c) {
            false => self.http.get_or_init(|| reach::Reach::new(false).client()),
            true => self.http_internal.get_or_init(|| reach::Reach::new(true).client()),
        }
    }

    /// Sets the prefix of the variables `env:` secrets may name
    /// (`--connector-env-prefix`): not empty, since an empty one would let
    /// a connector read the server's every variable, its tokens included.
    pub fn set_env_prefix(&self, prefix: &str) -> Result<(), String> {
        if prefix.len() < 4 || !prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("--connector-env-prefix {prefix:?}: four or more letters, digits and _ (default {ENV_PREFIX})"));
        }
        self.env_prefix.set(prefix.to_string()).map_err(|_| "the connectors' env prefix is set once".to_string())
    }

    /// The prefix of the variables `env:` secrets may name.
    pub fn env_prefix(&self) -> &str {
        self.env_prefix.get().map_or(ENV_PREFIX, String::as_str)
    }

    /// The value of the secret `env:VAR`, when `VAR` has the prefix; why
    /// not, else.
    fn env_secret(&self, var: &str) -> Result<Option<String>, String> {
        let prefix = self.env_prefix();
        if !var.starts_with(prefix) || var.len() == prefix.len() {
            return Err(format!("env:{var} is not a variable a connector may read: its name starts with {prefix} (the server's --connector-env-prefix), so that a connector reads no token, key or credential of the server's own"));
        }
        Ok(std::env::var(var).ok())
    }


    /// The names of `connector`'s secrets (never their values).
    fn secret_names(&self, connector: &str) -> Vec<String> {
        lock(&self.secrets).map.get(connector).map(|m| m.keys().cloned().collect()).unwrap_or_default()
    }

    /// `connector`'s secrets, `env:NAME` ones read from the environment
    /// (only those whose variable has the prefix: one kept from before the
    /// prefix is left out, and the log says why).
    fn secrets_of(&self, connector: &str) -> HashMap<String, String> {
        let s = lock(&self.secrets);
        s.map
            .get(connector)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| match v.strip_prefix("env:") {
                        Some(var) => match self.env_secret(var) {
                            Ok(x) => x.map(|x| (k.clone(), x)),
                            Err(why) => {
                                tracing::warn!(connector, secret = %k, "{why}");
                                None
                            }
                        },
                        None => Some((k.clone(), v.clone())),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Sets (a string), or takes out (null), `connector`'s secrets.
    fn set_secrets(&self, connector: &str, given: &Json) -> Result<(), ApiError> {
        let Json::Object(given) = given else { return Err(bad("secrets is {\"name\": \"value\"}, or \"env:VARIABLE\" for one the server reads from its environment")) };
        let mut s = lock(&self.secrets);
        let entry = s.map.entry(connector.to_string()).or_default();
        for (k, v) in given {
            if !name_ok(k) {
                return Err(bad(format!("{k:?} is not a secret's name: lowercase letters, digits, - and _")));
            }
            match v {
                Json::Null => {
                    entry.remove(k);
                }
                Json::String(x) if !x.is_empty() => {
                    if let Some(var) = x.strip_prefix("env:") {
                        self.env_secret(var).map_err(bad)?;
                    }
                    entry.insert(k.clone(), x.clone());
                }
                _ => return Err(bad(format!("secret {k} is text (or null to take it out)"))),
            }
        }
        if entry.is_empty() {
            s.map.remove(connector);
        }
        Self::save_secrets(&s)
    }

    fn drop_secrets(&self, connector: &str) -> Result<(), ApiError> {
        let mut s = lock(&self.secrets);
        if s.map.remove(connector).is_some() {
            Self::save_secrets(&s)?;
        }
        Ok(())
    }

    fn save_secrets(s: &Secrets) -> Result<(), ApiError> {
        let Some(f) = &s.file else { return Ok(()) };
        let body = serde_json::to_vec_pretty(&s.map).map_err(ApiError::internal)?;
        crate::aws::write_private(f, &body).map_err(|e| ApiError::internal(format!("write {}: {e}", f.display())))
    }

    /// A request to this server's own routes with `key` (none: as an open
    /// server's caller): its JSON answer, or why it failed.
    pub async fn call(&self, key: Option<&HeaderValue>, method: Method, path: &str, body: Option<Json>) -> Result<Json, String> {
        use tower::ServiceExt;
        let router = self.router.get().ok_or("the connectors are not started on this server")?;
        let host = self.host.get().map_or("localhost", String::as_str);
        let mut b = Request::builder().method(method).uri(path).header("x-strata-app", "connectors").header(header::HOST, host);
        if let Some(k) = key {
            b = b.header(header::AUTHORIZATION, k.clone());
        }
        let body = match body {
            Some(j) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&j).map_err(|e| e.to_string())?)
            }
            None => Body::empty(),
        };
        let req = b.body(body).map_err(|e| e.to_string())?;
        let res = match router.clone().oneshot(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 << 20).await.map_err(|e| format!("{status}: {e}"))?;
        let j: Json = serde_json::from_slice(&bytes).unwrap_or_else(|_| Json::String(String::from_utf8_lossy(&bytes).into_owned()));
        if !status.is_success() {
            return Err(format!("{}: {}", status.as_u16(), j["error"].as_str().map(str::to_string).unwrap_or_else(|| j.to_string())));
        }
        Ok(j)
    }

    fn bearer(secret: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {secret}")).unwrap_or_else(|_| HeaderValue::from_static(""))
    }

    /// The key the connectors' own tables are written with.
    fn own(&self, svc: &Service) -> Result<HeaderValue, String> {
        let mut k = lock(&self.own_key);
        if k.is_none() {
            let grants = vec![Grant::new("chloedb_connector*", Scope::Admin).map_err(|e| e.message)?];
            *k = Some(svc.keys.ephemeral_key("connectors", "made by this server for its connectors' own tables".into(), grants).map_err(|e| e.message)?);
        }
        Ok(Self::bearer(k.as_deref().unwrap_or_default()))
    }

    async fn own_sql(&self, svc: &Service, sql: &str, params: Json) -> Result<Json, String> {
        let k = self.own(svc)?;
        OWN_WRITE.scope(true, self.call(Some(&k), Method::POST, "/v1/sql", Some(json!({"sql": sql, "params": params, "objects": true})))).await
    }

    async fn own_write(&self, svc: &Service, ops: Vec<Json>) -> Result<Json, String> {
        let k = self.own(svc)?;
        OWN_WRITE.scope(true, self.call(Some(&k), Method::POST, "/v1/write", Some(json!({ "ops": ops })))).await
    }

    /// Makes the connectors' tables, once.
    async fn make_tables(&self, svc: &Service) -> Result<(), String> {
        if self.tables_made.load(Ordering::SeqCst) {
            return Ok(());
        }
        let have = svc.table_names();
        for (t, ddl) in SCHEMAS {
            if !have.iter().any(|h| h.eq_ignore_ascii_case(t)) {
                self.own_sql(svc, ddl, json!({})).await.map_err(|e| format!("making {t}: {e}"))?;
            }
        }
        // Who changed a config last: a column the tables of 0.3.3 and
        // before lack.
        if let Ok(s) = svc.table(CONFIGS).await {
            if !s.fields.iter().any(|f| f.name == "updated_by") {
                self.own_sql(svc, &format!("ALTER TABLE {CONFIGS} ADD COLUMN updated_by KEYWORD"), json!({})).await.map_err(|e| format!("adding updated_by to {CONFIGS}: {e}"))?;
            }
        }
        self.tables_made.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn has_tables(svc: &Service) -> bool {
        svc.table_names().iter().any(|t| t.eq_ignore_ascii_case(CONFIGS))
    }

    /// Every connector, read again when the last read is older than 10 s
    /// (or `fresh`).
    pub async fn list(&self, svc: &Service, fresh: bool) -> Result<Vec<Connector>, String> {
        if !fresh {
            if let Some((at, v)) = lock(&self.cache).as_ref() {
                if now_ms().saturating_sub(*at) < 10_000 {
                    return Ok(v.clone());
                }
            }
        }
        if !Self::has_tables(svc) {
            return Ok(Vec::new());
        }
        let j = self.own_sql(svc, &format!("SELECT * FROM {CONFIGS} LIMIT 1000"), json!({})).await?;
        let mut v: Vec<Connector> = j["rows"].as_array().into_iter().flatten().filter_map(Connector::from_row).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        *lock(&self.cache) = Some((now_ms(), v.clone()));
        self.load_state(svc).await;
        Ok(v)
    }

    fn forget_cache(&self) {
        *lock(&self.cache) = None;
    }

    async fn find(&self, svc: &Service, name: &str) -> Result<Connector, ApiError> {
        let list = self.list(svc, true).await.map_err(ApiError::internal)?;
        list.into_iter().find(|c| c.name == name).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no connector named {name}") })
    }

    async fn load_state(&self, svc: &Service) {
        if self.state_loaded.load(Ordering::SeqCst) || !svc.table_names().iter().any(|t| t.eq_ignore_ascii_case(STATE)) {
            return;
        }
        if let Ok(j) = self.own_sql(svc, &format!("SELECT * FROM {STATE} LIMIT 1000"), json!({})).await {
            let mut st = lock(&self.state);
            for r in j["rows"].as_array().into_iter().flatten() {
                if let Some(n) = r["id"].as_str() {
                    st.entry(n.to_string()).or_insert_with(|| State::from_row(r));
                }
            }
            self.state_loaded.store(true, Ordering::SeqCst);
        }
    }

    fn state_of(&self, name: &str) -> State {
        lock(&self.state).get(name).cloned().unwrap_or_default()
    }

    /// Starts the schedule's loop, once, when pulls run here.
    pub fn start(svc: &Arc<Service>, router: Router, host: Option<String>) {
        let me = &svc.connectors;
        let _ = me.router.set(router);
        if let Some(h) = host {
            let _ = me.host.set(h);
        }
        if !me.enabled() || me.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let svc = svc.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = svc.connectors.wake.notified() => {}
                }
                svc.connectors.tick(&svc).await;
            }
        });
    }

    /// Starts the pulls that are due.
    async fn tick(&self, svc: &Arc<Service>) {
        let (me, pager) = svc.alerts.pager();
        if !me.is_empty() && me != pager {
            return;
        }
        let Ok(list) = self.list(svc, false).await else { return };
        let now = now_ms();
        // A row that fails the API's checks (written past it) never runs.
        for c in list.into_iter().filter(|c| c.enabled && c.pulls() && c.invalid.is_none()) {
            let st = self.state_of(&c.name);
            let wait_s = if st.more {
                0
            } else if st.failures > 0 {
                (c.every_s.saturating_mul(1 << st.failures.min(5))).min(3600).max(c.every_s)
            } else {
                c.every_s
            };
            if st.last_run_ms + wait_s * 1000 > now {
                continue;
            }
            if !lock(&self.running).insert(c.name.clone()) {
                continue;
            }
            let svc = svc.clone();
            tokio::spawn(async move {
                let name = c.name.clone();
                let _ = tokio::time::timeout(Duration::from_secs(600), svc.connectors.run(&svc, &c, Trigger::Schedule, None)).await;
                lock(&svc.connectors.running).remove(&name);
            });
        }
    }

    /// Records, for a push: the body and its type, and the credential it
    /// came with.
    async fn run(&self, svc: &Service, c: &Connector, trigger: Trigger, push: Option<(&[u8], &str, Option<&HeaderValue>)>) -> Json {
        let t0 = std::time::Instant::now();
        let started_ms = now_ms();
        let st = self.state_of(&c.name);
        let secrets = self.secrets_of(&c.name);
        let now_us = started_ms as i64 * 1000;
        let reach = kinds::reach_of(c);
        let ctx = kinds::Ctx { c, cursor: st.cursor.clone(), secrets: &secrets, http: self.http(c), reach: &reach, now_us, limit: RUN_LIMIT, max_bytes: kinds::max_bytes(c) };
        // Checked again here: the row may have been written past the API.
        let fetched = match (c.check(), push) {
            (Err(why), _) => Err(format!("connector {} does not run: {why} (change it with POST /v1/admin/connectors/{}, or delete it)", c.name, c.name)),
            (Ok(()), Some((body, ty, _))) => kinds::push(&ctx, body, ty).await.and_then(|records| match records.len() > RUN_LIMIT {
                true => Err(format!("{TOO_MANY} {RUN_LIMIT} records at most, and this one has {}: send it in parts", records.len())),
                false => Ok(kinds::Fetched { records, ..Default::default() }),
            }),
            (Ok(()), None) => kinds::pull(&ctx).await,
        };
        let mut report = json!({"connector": c.name, "kind": c.kind, "target": c.target, "trigger": trigger.name(), "started_ms": started_ms});
        let outcome: Result<(usize, usize, usize, Vec<String>, Option<String>, bool), String> = async {
            let f = fetched?;
            let records = f.records.len();
            let (rows, skipped, dropped, first_err) = self.rows_of(svc, c, &f.records, now_us).await?;
            let key_owned;
            let key: Option<&HeaderValue> = match push {
                Some((_, _, k)) => k,
                None => {
                    let name = format!("connector-run:{}", c.name);
                    // One table by its name: never a pattern (`*`, `x*`).
                    if !table_ok(&c.target) || own_table(&c.target) {
                        return Err(format!("{:?} is not a table a connector may write", c.target));
                    }
                    let grants = vec![Grant::new(&c.target, Scope::Write).map_err(|e| e.message)?];
                    key_owned = Self::bearer(&svc.keys.ephemeral_key(&name, format!("made by this server for a run of connector {}", c.name), grants).map_err(|e| e.message)?);
                    Some(&key_owned)
                }
            };
            let keyed = svc.table(&c.target).await.map(|s| s.primary_key.is_some()).unwrap_or(true);
            let op = if keyed { "upsert" } else { "insert" };
            let n = rows.len();
            let mut written = 0usize;
            for chunk in batches(&rows) {
                let ops: Vec<Json> = chunk.iter().map(|r| json!({"op": op, "table": c.target, "doc": r})).collect();
                if let Err(e) = self.call(key, Method::POST, "/v1/write", Some(json!({ "ops": ops }))).await {
                    if push.is_none() {
                        svc.keys.end_ephemeral(&format!("connector-run:{}", c.name));
                    }
                    return Err(format!("writing {} (after {written} of {n} rows): {e}", c.target));
                }
                written += chunk.len();
            }
            if push.is_none() {
                svc.keys.end_ephemeral(&format!("connector-run:{}", c.name));
            }
            if let Some(e) = &first_err {
                report["first_skip"] = json!(kinds::redact(e, &secrets));
            }
            Ok((records, n, skipped, dropped, f.cursor, f.more))
        }
        .await
        .map_err(|e| kinds::redact(&e, &secrets));
        let took_ms = t0.elapsed().as_millis() as u64;
        let mut st = self.state_of(&c.name);
        st.runs += 1;
        st.last_run_ms = started_ms;
        let (ok, error) = match &outcome {
            Ok((records, rows, skipped, dropped, cursor, more)) => {
                report["records"] = json!(records);
                report["rows"] = json!(rows);
                report["skipped"] = json!(skipped);
                report["dropped"] = json!(dropped);
                report["more"] = json!(more);
                st.last_ok_ms = started_ms;
                st.failures = 0;
                st.last_error = None;
                st.records_total += *records as u64;
                st.rows_total += *rows as u64;
                st.last_rows = *rows as u64;
                st.more = *more;
                // None keeps the last; an empty one clears it.
                if let Some(cur) = cursor {
                    st.cursor = Some(cur.clone()).filter(|c| !c.is_empty());
                }
                (true, None)
            }
            Err(e) => {
                st.failures += 1;
                st.last_error = Some(e.clone());
                st.more = false;
                (false, Some(e.clone()))
            }
        };
        report["ok"] = json!(ok);
        report["error"] = json!(error);
        report["took_ms"] = json!(took_ms);
        lock(&self.state).insert(c.name.clone(), st.clone());
        let id = format!("{}/{started_ms}/{}", c.name, crate::keys::random_hex(3).unwrap_or_default());
        let run_row = json!({
            "id": id, "connector": c.name, "kind": c.kind, "target": c.target, "trigger": trigger.name(),
            "started": chloedb::timestamp::format(started_ms as i64 * 1000), "ended": chloedb::timestamp::format(now_ms() as i64 * 1000),
            "took_ms": took_ms, "ok": ok, "records": report["records"], "rows_": report["rows"], "skipped": report["skipped"],
            "dropped": report["dropped"], "error": error.unwrap_or_default(), "cursor": st.cursor.clone().unwrap_or_default(), "more": st.more,
        });
        let ops = vec![json!({"op": "upsert", "table": STATE, "doc": st.row(&c.name)}), json!({"op": "insert", "table": RUNS, "doc": run_row})];
        if let Err(e) = self.own_write(svc, ops).await {
            tracing::warn!(connector = %c.name, error = %e, "cannot record a connector's run");
        }
        match &report["error"] {
            Json::String(e) => tracing::warn!(connector = %c.name, kind = %c.kind, trigger = trigger.name(), error = %e, "connector run failed"),
            _ => tracing::info!(connector = %c.name, kind = %c.kind, trigger = trigger.name(), rows = report["rows"].as_u64().unwrap_or(0), took_ms, "connector run"),
        }
        report
    }

    /// The rows `records` make for `c`'s target: the rows, how many
    /// records made none, the columns dropped, and the first record's
    /// error.
    async fn rows_of(&self, svc: &Service, c: &Connector, records: &[Json], now_us: i64) -> Result<(Vec<Map<String, Json>>, usize, Vec<String>, Option<String>), String> {
        let schema = svc.table(&c.target).await.map_err(|e| format!("the target table {}: {}", c.target, e.message))?;
        let m = kinds::map_of(c);
        let mut fitted = map::Fitted::default();
        let mut rows = Vec::with_capacity(records.len());
        let mut skipped = 0;
        let mut first = None;
        for r in records {
            match map::row(r, m.as_ref(), now_us).and_then(|row| map::fit(row, &schema, &mut fitted)) {
                Ok(row) => rows.push(row),
                Err(e) => {
                    skipped += 1;
                    first.get_or_insert(e);
                }
            }
        }
        if rows.is_empty() && skipped > 0 {
            return Err(format!("none of the {skipped} records made a row: {}", first.unwrap_or_default()));
        }
        Ok((rows, skipped, fitted.dropped, first))
    }

    /// A connector's health ([`self`]).
    fn health(&self, c: &Connector, st: &State) -> &'static str {
        if c.invalid.is_some() {
            return "invalid";
        }
        if !c.enabled {
            return "disabled";
        }
        if lock(&self.running).contains(&c.name) {
            return "running";
        }
        if st.runs == 0 {
            return if c.pulls() { "pending" } else { "waiting" };
        }
        if st.failures > 0 {
            return "failing";
        }
        if c.pulls() {
            let late = (c.every_s * 3).max(900) * 1000;
            if now_ms().saturating_sub(st.last_ok_ms) > late {
                return "stale";
            }
        }
        "ok"
    }

    fn json_of(&self, c: &Connector) -> Json {
        let st = self.state_of(&c.name);
        let health = self.health(c, &st);
        let next_ms = (c.enabled && c.pulls()).then(|| if st.last_run_ms == 0 { now_ms() } else { st.last_run_ms + c.every_s * 1000 });
        json!({
            "name": c.name, "kind": c.kind, "target": c.target, "enabled": c.enabled, "every_s": c.every_s, "config": c.config,
            "note": c.note, "created_ms": c.created_ms, "updated_ms": c.updated_ms, "updated_by": c.updated_by, "invalid": c.invalid, "pulls": c.pulls(), "pushed": c.pushed(),
            "push": c.pushed().then(|| json!({"url": format!("/v1/connectors/{}/push", c.name), "key_id": c.push_key})),
            "secrets": self.secret_names(&c.name), "health": health, "state": st, "next_ms": next_ms,
        })
    }
}

/// What the API's handlers know of a request: its credential and whether
/// the server is guarded (by a token) or in a cluster.
pub struct Caller {
    pub auth: Option<HeaderValue>,
    pub guarded: bool,
    pub in_cluster: bool,
    pub who: String,
}

fn kinds_json() -> Json {
    Json::Array(
        kinds::KINDS
            .iter()
            .map(|k| {
                let c = Connector { name: String::new(), kind: k.name.into(), target: String::new(), enabled: true, every_s: 0, config: json!({}), push_key: None, note: String::new(), created_ms: 0, updated_ms: 0, updated_by: String::new(), invalid: None };
                json!({"kind": k.name, "title": k.title, "about": k.about, "pull": k.pull, "push": k.push, "target": kinds::default_target(&c), "every_s": default_every(k.name), "settings": k.settings.iter().map(|(n, w)| json!({"name": n, "about": w})).collect::<Vec<_>>(), "secrets": k.secrets})
            })
            .collect(),
    )
}

fn default_every(kind: &str) -> u64 {
    match kind {
        "prometheus" => 60,
        "servicenow" | "jira" | "http_json" => 300,
        "oxidized" => 900,
        _ => 0,
    }
}

/// `GET /v1/admin/connectors`: every connector with its health, state and
/// last runs; the kinds; whether pulls run here.
pub async fn list_json(svc: &Arc<Service>, caller: &Caller) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let list = me.list(svc, true).await.map_err(ApiError::internal)?;
    let mut runs: HashMap<String, Vec<Json>> = HashMap::new();
    if svc.table_names().iter().any(|t| t.eq_ignore_ascii_case(RUNS)) {
        if let Ok(j) = me.own_sql(svc, &format!("SELECT * FROM {RUNS} ORDER BY started DESC LIMIT 500"), json!({})).await {
            for r in j["rows"].as_array().into_iter().flatten() {
                let v = runs.entry(r["connector"].as_str().unwrap_or_default().to_string()).or_default();
                if v.len() < 12 {
                    v.push(r.clone());
                }
            }
        }
    }
    let connectors: Vec<Json> = list
        .iter()
        .map(|c| {
            let mut j = me.json_of(c);
            j["runs"] = Json::Array(runs.remove(&c.name).unwrap_or_default());
            j
        })
        .collect();
    let mut by_health: BTreeMap<String, u64> = BTreeMap::new();
    for c in &connectors {
        *by_health.entry(c["health"].as_str().unwrap_or_default().to_string()).or_default() += 1;
    }
    // The other ways rows come in: stream sources and followed databases.
    let sources = me.call(caller.auth.as_ref(), Method::GET, "/v1/sources", None).await.ok().and_then(|j| j.get("sources").cloned()).unwrap_or(json!([]));
    let (here, pager) = svc.alerts.pager();
    let scheduler = if !me.enabled() {
        json!({"on": false, "why": "pulls do not run on this server (a replica, a shard, or --connectors off): pushes are taken, and runs started by hand"})
    } else if !here.is_empty() && here != pager {
        json!({"on": false, "why": format!("another coordinator ({pager}) runs the pulls")})
    } else {
        json!({"on": true})
    };
    Ok(json!({"connectors": connectors, "kinds": kinds_json(), "health": by_health, "scheduler": scheduler, "sources": sources, "tables": {"configs": CONFIGS, "runs": RUNS, "state": STATE}, "at_ms": now_ms()}))
}

/// `GET /v1/admin/connectors/:name`.
pub async fn one_json(svc: &Arc<Service>, name: &str) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let c = me.find(svc, name).await?;
    let mut j = me.json_of(&c);
    let runs = me.own_sql(svc, &format!("SELECT * FROM {RUNS} WHERE connector = :n ORDER BY started DESC LIMIT 50"), json!({"n": name})).await.map(|j| j["rows"].clone()).unwrap_or(json!([]));
    j["runs"] = runs;
    j["map"] = kinds::map_of(&c).map_or(Json::Null, Json::Object);
    Ok(j)
}

/// A connector from a request's body (`old`'s settings where it gives
/// none).
fn spec(b: &Json, old: Option<&Connector>) -> Result<Connector, ApiError> {
    let name = match old {
        Some(o) => o.name.clone(),
        None => b["name"].as_str().map(str::trim).unwrap_or_default().to_string(),
    };
    if !name_ok(&name) {
        return Err(bad(format!("{name:?} is not a connector's name: 1 to 48 lowercase letters, digits, - and _")));
    }
    let kind = match (b["kind"].as_str(), old) {
        (Some(k), _) => k.to_string(),
        (None, Some(o)) => o.kind.clone(),
        (None, None) => return Err(bad(format!("a connector names its kind: {}", kinds::KINDS.iter().map(|k| k.name).collect::<Vec<_>>().join(", ")))),
    };
    let k = kinds::kind(&kind).ok_or_else(|| bad(format!("{kind:?} is not a kind of connector: {}", kinds::KINDS.iter().map(|k| k.name).collect::<Vec<_>>().join(", "))))?;
    let config = match (&b["config"], old) {
        (Json::Object(_), _) => b["config"].clone(),
        (Json::Null, Some(o)) => o.config.clone(),
        (Json::Null, None) => json!({}),
        _ => return Err(bad("config is an object")),
    };
    let mut c = Connector {
        name,
        kind: kind.clone(),
        target: String::new(),
        enabled: b["enabled"].as_bool().or(old.map(|o| o.enabled)).unwrap_or(true),
        every_s: b["every_s"].as_u64().or(old.map(|o| o.every_s)).unwrap_or_else(|| default_every(&kind)),
        config,
        push_key: old.and_then(|o| o.push_key.clone()),
        note: b["note"].as_str().map(str::to_string).or(old.map(|o| o.note.clone())).unwrap_or_default(),
        created_ms: old.map_or(now_ms(), |o| o.created_ms),
        updated_ms: now_ms(),
        updated_by: old.map(|o| o.updated_by.clone()).unwrap_or_default(),
        invalid: None,
    };
    c.target = match (b["target"].as_str().map(str::trim).filter(|t| !t.is_empty()), old) {
        (Some(t), _) => t.to_ascii_lowercase(),
        (None, Some(o)) => o.target.clone(),
        (None, None) => kinds::default_target(&c),
    };
    if !k.pull {
        c.every_s = 0;
    }
    c.check().map_err(bad)?;
    Ok(c)
}

/// Makes `c`'s target when it has none, with the caller's credential: with
/// the columns of its own map ([`kinds::map_schema`]) when it has one and
/// writes a table of its choosing, else with its kind's default table.
async fn make_target(svc: &Arc<Service>, c: &Connector, caller: &Caller) -> Result<bool, ApiError> {
    if svc.table(&c.target).await.is_ok() {
        return Ok(false);
    }
    if c.target != kinds::default_target(c) && c.config["create_target"] != true {
        return Err(ApiError { code: None, status: 404, message: format!("there is no table {}: make it first, or write the kind's own table ({}), or pass \"create_target\": true in config to make {} with that table's columns", c.target, kinds::default_target(c), c.target) });
    }
    // Its own map's columns, else its kind's table.
    let ddl = match &c.config["map"] {
        Json::Object(m) if !m.is_empty() => kinds::map_schema(m, &c.target).map_err(bad)?,
        _ => kinds::default_schema(&c.kind, &c.target),
    };
    svc.connectors.call(caller.auth.as_ref(), Method::POST, "/v1/sql", Some(json!({ "sql": ddl }))).await.map_err(|e| ApiError { code: None, status: 400, message: format!("making {}: {e}", c.target) })?;
    Ok(true)
}

/// The push key of `c`: a key of the keys file limited to writing its
/// target. Its secret, or why there is none.
fn push_key(svc: &Service, c: &mut Connector, caller: &Caller) -> Result<String, String> {
    if let Some(old) = c.push_key.take() {
        let _ = svc.keys.revoke_by(&old, caller.guarded, Some(caller.who.clone()));
    }
    if !caller.guarded && svc.keys.is_empty() {
        return Err("the server is open (no token and no key): a push needs no key".into());
    }
    let grants = vec![Grant::new(&c.target, Scope::Write).map_err(|e| e.message)?];
    let name = format!("{}{}", crate::keys::PUSH_KEY_PREFIX, c.name);
    // A key of that name left by a connector of that name deleted by SQL.
    for k in svc.keys.set().keys {
        if k.name == name {
            let _ = svc.keys.revoke_by(&k.id, caller.guarded, Some(caller.who.clone()));
        }
    }
    let (key, secret) = svc.keys.create_limited(&name, Scope::Write, Some(grants), None, Some(name.clone()), Some(format!("push key of connector {} ({}): writes {}", c.name, c.kind, c.target)), caller.guarded, caller.in_cluster).map_err(|e| e.message)?;
    c.push_key = Some(key.id);
    Ok(secret)
}

/// `POST /v1/admin/connectors`: a new connector (409 if the name is
/// taken). Its push key's secret, if it has one, is in this answer only.
pub async fn create(svc: &Arc<Service>, caller: &Caller, b: &Json) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let mut c = spec(b, None)?;
    c.updated_by = caller.who.clone();
    me.make_tables(svc).await.map_err(ApiError::internal)?;
    if me.list(svc, true).await.map_err(ApiError::internal)?.iter().any(|x| x.name == c.name) {
        return Err(ApiError { code: None, status: 409, message: format!("a connector named {} exists: POST /v1/admin/connectors/{} changes it", c.name, c.name) });
    }
    let made = make_target(svc, &c, caller).await?;
    if let Some(s) = b.get("secrets").filter(|s| !s.is_null()) {
        me.set_secrets(&c.name, s)?;
    }
    let mut out = json!({});
    if c.pushed() {
        match push_key(svc, &mut c, caller) {
            Ok(secret) => out["push_secret"] = json!(secret),
            Err(why) => out["push_key_error"] = json!(why),
        }
    }
    me.own_write(svc, vec![json!({"op": "upsert", "table": CONFIGS, "doc": c.row()})]).await.map_err(ApiError::internal)?;
    me.forget_cache();
    me.wake.notify_one();
    out["connector"] = me.json_of(&c);
    out["made_target"] = json!(made);
    Ok(out)
}

/// `POST /v1/admin/connectors/:name`: changes a connector's settings
/// (those the body gives; `secrets` merged, a null taking one out). A
/// push connector whose target changes gets a new key.
pub async fn update(svc: &Arc<Service>, caller: &Caller, name: &str, b: &Json) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let old = me.find(svc, name).await?;
    let mut c = spec(b, Some(&old))?;
    c.updated_by = caller.who.clone();
    me.make_tables(svc).await.map_err(ApiError::internal)?;
    make_target(svc, &c, caller).await?;
    if let Some(s) = b.get("secrets").filter(|s| !s.is_null()) {
        me.set_secrets(&c.name, s)?;
    }
    let mut out = json!({});
    if c.pushed() && (c.target != old.target || c.push_key.is_none() && caller.guarded) {
        match push_key(svc, &mut c, caller) {
            Ok(secret) => out["push_secret"] = json!(secret),
            Err(why) => out["push_key_error"] = json!(why),
        }
    }
    me.own_write(svc, vec![json!({"op": "upsert", "table": CONFIGS, "doc": c.row()})]).await.map_err(ApiError::internal)?;
    me.forget_cache();
    me.wake.notify_one();
    out["connector"] = me.json_of(&c);
    Ok(out)
}

/// `DELETE /v1/admin/connectors/:name`: its row, its state, its secrets
/// and its push key (its runs stay, for their 30 days).
pub async fn delete(svc: &Arc<Service>, caller: &Caller, name: &str) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let c = me.find(svc, name).await?;
    if let Some(k) = &c.push_key {
        let _ = svc.keys.revoke_by(k, caller.guarded, Some(caller.who.clone()));
    }
    me.own_write(svc, vec![json!({"op": "delete", "table": CONFIGS, "key": c.name}), json!({"op": "delete", "table": STATE, "key": c.name})]).await.map_err(ApiError::internal)?;
    me.drop_secrets(&c.name)?;
    lock(&me.state).remove(&c.name);
    me.forget_cache();
    Ok(json!({"deleted": c.name}))
}

/// `POST /v1/admin/connectors/:name/key`: a new push key in place of the
/// old (revoked now), its secret in this answer only.
pub async fn rotate(svc: &Arc<Service>, caller: &Caller, name: &str) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let mut c = me.find(svc, name).await?;
    if !c.pushed() {
        return Err(bad(format!("a {} connector is not pushed to, so it has no push key", c.kind)));
    }
    me.make_tables(svc).await.map_err(ApiError::internal)?;
    let secret = push_key(svc, &mut c, caller).map_err(|e| ApiError { code: None, status: 409, message: e })?;
    me.own_write(svc, vec![json!({"op": "upsert", "table": CONFIGS, "doc": c.row()})]).await.map_err(ApiError::internal)?;
    me.forget_cache();
    Ok(json!({"connector": c.name, "key_id": c.push_key, "push_secret": secret}))
}

/// `POST /v1/admin/connectors/:name/run`: a pull now, its report.
pub async fn run_now(svc: &Arc<Service>, name: &str) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let c = me.find(svc, name).await?;
    if let Some(why) = &c.invalid {
        return Err(ApiError { code: None, status: 409, message: format!("connector {name} does not run: {why} (change it with POST /v1/admin/connectors/{name}, or delete it)") });
    }
    if !kinds::kind(&c.kind).is_some_and(|k| k.pull) {
        return Err(bad(format!("a {} connector is not pulled: systems push to it (POST /v1/connectors/{}/push)", c.kind, c.name)));
    }
    if !lock(&me.running).insert(c.name.clone()) {
        return Err(ApiError { code: None, status: 409, message: format!("connector {} is running now", c.name) });
    }
    let r = me.run(svc, &c, Trigger::Manual, None).await;
    lock(&me.running).remove(&c.name);
    Ok(r)
}

/// `POST /v1/admin/connectors/test`: what a connector would read and
/// write, nothing written: `{name?, kind, target?, config, secrets?,
/// sample?}` (a saved connector's own when `name` names one and the body
/// gives no more; `sample` is a push's body for a pushed kind). Its rows
/// as they would be written, the first ten.
pub async fn test(svc: &Arc<Service>, b: &Json) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let saved = match b["name"].as_str() {
        Some(n) if !n.is_empty() => me.list(svc, true).await.map_err(ApiError::internal)?.into_iter().find(|c| c.name == n),
        _ => None,
    };
    let mut body = b.clone();
    if body["name"].as_str().is_none_or(str::is_empty) {
        body["name"] = json!("test");
    }
    let c = spec(&body, saved.as_ref())?;
    let mut secrets = me.secrets_of(&c.name);
    if let Some(Json::Object(given)) = b.get("secrets") {
        for (k, v) in given {
            if let Some(v) = v.as_str() {
                // A dry run keeps nothing and is not audited as a change:
                // it is given values, and reads none of the server's.
                if v.starts_with("env:") {
                    return Err(bad(format!("secret {k}: a dry run takes a secret's value, not env:VARIABLE (a saved connector reads its env: secrets)")));
                }
                secrets.insert(k.clone(), v.to_string());
            }
        }
    }
    let now_us = now_ms() as i64 * 1000;
    let cursor = saved.as_ref().and_then(|s| me.state_of(&s.name).cursor);
    let reach = kinds::reach_of(&c);
    let ctx = kinds::Ctx { c: &c, cursor, secrets: &secrets, http: me.http(&c), reach: &reach, now_us, limit: 200, max_bytes: kinds::max_bytes(&c) };
    let t0 = std::time::Instant::now();
    let fetched = match b.get("sample").filter(|s| !s.is_null()) {
        Some(Json::String(raw)) => kinds::push(&ctx, raw.as_bytes(), if c.kind == "mail" { "message/rfc822" } else { "application/json" }).await.map(|records| kinds::Fetched { records, ..Default::default() }),
        Some(j) => kinds::push(&ctx, j.to_string().as_bytes(), "application/json").await.map(|records| kinds::Fetched { records, ..Default::default() }),
        None if kinds::kind(&c.kind).is_some_and(|k| k.pull) => kinds::pull(&ctx).await,
        None => Err(format!("a {} connector is tested with a \"sample\" of what a system sends it", c.kind)),
    };
    let f = fetched.map_err(|e| ApiError { code: None, status: 502, message: e })?;
    let target = svc.table(&c.target).await.ok();
    let m = kinds::map_of(&c);
    let mut fitted = map::Fitted::default();
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for r in f.records.iter().take(10) {
        match map::row(r, m.as_ref(), now_us) {
            Ok(row) => match &target {
                Some(s) => match map::fit(row, s, &mut fitted) {
                    Ok(r) => rows.push(Json::Object(r)),
                    Err(e) => errors.push(json!(e)),
                },
                None => rows.push(Json::Object(row)),
            },
            Err(e) => errors.push(json!(e)),
        }
    }
    Ok(json!({
        "connector": c.name, "kind": c.kind, "target": c.target, "target_exists": target.is_some(),
        "records": f.records.len(), "cursor": f.cursor, "more": f.more, "took_ms": t0.elapsed().as_millis() as u64,
        "sample_records": f.records.iter().take(3).cloned().collect::<Vec<_>>(), "rows": rows, "errors": errors, "dropped": fitted.dropped,
        "map": m.map_or(Json::Null, Json::Object),
    }))
}

/// `POST /v1/connectors/:name/push`: a system's push, written with its
/// own credential (the connector's push key, or any key that may write
/// the target).
pub async fn push(svc: &Arc<Service>, name: &str, auth: Option<&HeaderValue>, content_type: &str, body: &[u8]) -> Result<Json, ApiError> {
    let me = &svc.connectors;
    let c = me.list(svc, false).await.map_err(ApiError::internal)?.into_iter().find(|c| c.name == name);
    let c = match c {
        Some(c) => c,
        None => me.find(svc, name).await?,
    };
    if !c.pushed() {
        return Err(bad(format!("a {} connector is not pushed to: it pulls on its schedule", c.kind)));
    }
    if let Some(why) = &c.invalid {
        return Err(ApiError { code: None, status: 409, message: format!("connector {name} takes no push: {why}") });
    }
    if !c.enabled {
        return Err(ApiError { code: None, status: 409, message: format!("connector {name} is disabled") });
    }
    let r = me.run(svc, &c, Trigger::Push, Some((body, content_type, auth))).await;
    match r["error"].as_str() {
        None => Ok(r),
        Some(e) => {
            let status = if e.starts_with(TOO_MANY) {
                413
            } else if e.contains("403:") {
                403
            } else if e.contains("401:") {
                401
            } else {
                400
            };
            Err(ApiError { code: None, status, message: e.to_string() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_take_defaults_and_refuse_what_they_cannot_run() {
        let c = spec(&json!({"name": "prom", "kind": "prometheus", "config": {"url": "http://p:9090", "queries": {"cpu": "up"}}}), None).unwrap();
        assert_eq!((c.target.as_str(), c.every_s, c.enabled), ("prometheus_rollups", 60, true));
        let w = spec(&json!({"name": "am", "kind": "webhook", "config": {"preset": "alertmanager"}, "every_s": 60}), None).unwrap();
        assert_eq!((w.target.as_str(), w.every_s), ("alertmanager_alerts", 0));
        assert!(spec(&json!({"name": "Bad Name", "kind": "jira"}), None).is_err());
        assert!(spec(&json!({"name": "x", "kind": "ftp"}), None).is_err());
        assert!(spec(&json!({"name": "x", "kind": "jira", "target": "chloedb_connectors"}), None).is_err());
        assert!(spec(&json!({"name": "x", "kind": "jira", "every_s": 2}), None).is_err());
        assert!(spec(&json!({"name": "x", "kind": "webhook", "config": {"map": {"id": {"path": "$.a", "as": "nope"}}}}), None).is_err());
        // An update keeps what it does not give.
        let u = spec(&json!({"enabled": false}), Some(&c)).unwrap();
        assert_eq!((u.name.as_str(), u.kind.as_str(), u.enabled, u.every_s), ("prom", "prometheus", false, 60));
    }

    #[test]
    fn rows_and_state_round_trip() {
        let c = spec(&json!({"name": "j", "kind": "jira", "config": {"url": "https://x.atlassian.net", "jql": "project = NET"}, "note": "changes"}), None).unwrap();
        let back = Connector::from_row(&c.row()).unwrap();
        assert_eq!((back.name, back.kind, back.target, back.config, back.note), (c.name, c.kind, c.target, c.config, c.note));
        let st = State { cursor: Some("2026-11-13T03:00:00Z".into()), last_run_ms: 1_794_538_800_000, failures: 2, rows_total: 9, ..Default::default() };
        let b = State::from_row(&st.row("j"));
        assert_eq!((b.cursor, b.last_run_ms, b.failures, b.rows_total), (st.cursor, st.last_run_ms, 2, 9));
    }
}
