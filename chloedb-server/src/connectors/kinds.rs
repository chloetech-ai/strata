//! The kinds of connector: what each reads, how, and the table and map it
//! writes by default.
//!
//! A pull connector ([`pull`]) asks a system on a schedule for what
//! changed since its cursor and answers records and its next cursor; a
//! push connector ([`push`]) turns a request a system sent into records.
//! Each kind has a default target table (made when the connector is, if
//! missing) and a default map into it, and some have presets for the
//! network operations app's tables ([`preset_map`]).

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Map, Value as Json};

use super::map;
use super::reach::{self, same_origin};
use super::Connector;

/// What a kind is, for the API and the command center's form.
pub struct Kind {
    pub name: &'static str,
    pub title: &'static str,
    pub about: &'static str,
    pub pull: bool,
    pub push: bool,
    /// The settings it reads from `config`, each with what it is.
    pub settings: &'static [(&'static str, &'static str)],
    /// The secrets it reads, by name.
    pub secrets: &'static [&'static str],
    /// Its default target's name.
    pub target: &'static str,
}

pub const KINDS: &[Kind] = &[
    Kind {
        name: "webhook",
        title: "Webhook",
        about: "Any system that posts JSON: each request's records (a list in the body, or the body) become rows. Presets: alertmanager, and any target with a map.",
        pull: false,
        push: true,
        settings: &[("records", "path to the list of records in a request (default: the body, or its list)"), ("preset", "alertmanager: Alertmanager's webhook (alerts[*])"), ("map", "column -> path, template or value")],
        secrets: &[],
        target: "webhook_events",
    },
    Kind {
        name: "http_json",
        title: "HTTP JSON pull",
        about: "Any REST API that answers JSON, asked on a schedule: the records at a path, the next page at another, a cursor kept from the records.",
        pull: true,
        push: false,
        settings: &[("url", "the URL; {cursor} is replaced by the cursor"), ("records", "path to the records"), ("next", "path to the next page's URL (optional)"), ("cursor", "path in a record whose highest value is the next cursor (optional)"), ("start", "the first cursor"), ("headers", "headers; {secret:NAME} is a secret"), ("auth", "basic (username + secret password) or bearer (secret token)"), ("map", "column -> path, template or value"), ("allow_internal", "true to reach a system inside this network (a private address, this machine); never cloud metadata"), ("max_body_mb", "the largest answer read, MiB (default 32, at most 1024)")],
        secrets: &["token", "password"],
        target: "http_records",
    },
    Kind {
        name: "prometheus",
        title: "Prometheus rollups",
        about: "PromQL queries asked over each new minute (query_range), their series joined by labels into one row per key and step: the per-minute rollups an incident needs, the full series staying in Prometheus.",
        pull: true,
        push: false,
        settings: &[("url", "Prometheus (or Thanos, Mimir, VictoriaMetrics) base URL"), ("queries", "{column: PromQL}"), ("key", "the labels that name a row's subject (default: every label)"), ("rename", "{label: column}"), ("step_s", "the step, seconds (default 60)"), ("lag_s", "how far behind now to stay (default 60)"), ("backfill_s", "how far back the first run reads (default 3600)"), ("auth", "basic or bearer"), ("allow_internal", "true to reach a system inside this network (a private address, this machine); never cloud metadata"), ("max_body_mb", "the largest answer read, MiB (default 32, at most 1024)")],
        secrets: &["token", "password"],
        target: "prometheus_rollups",
    },
    Kind {
        name: "servicenow",
        title: "ServiceNow",
        about: "Records of a ServiceNow table (change_request, incident, problem...) updated since the last run, through the Table API.",
        pull: true,
        push: false,
        settings: &[("instance", "https://<instance>.service-now.com"), ("table", "change_request, incident, ..."), ("query", "an encoded query that narrows them (optional)"), ("fields", "the fields to read (optional, comma separated)"), ("username", "the integration user (basic auth)"), ("backfill_days", "how far back the first run reads (default 30)"), ("allow_internal", "true to reach a system inside this network (a private address, this machine); never cloud metadata"), ("max_body_mb", "the largest answer read, MiB (default 32, at most 1024)")],
        secrets: &["password", "token"],
        target: "servicenow_records",
    },
    Kind {
        name: "jira",
        title: "Jira",
        about: "Issues matching a JQL query updated since the last run, through Jira Cloud's or Data Center's search API.",
        pull: true,
        push: false,
        settings: &[("url", "https://<site>.atlassian.net or your Jira's base URL"), ("jql", "which issues (project = NET AND issuetype = Change)"), ("username", "the account's email (Cloud, basic auth with an API token)"), ("fields", "{name: field id} for custom fields (window_start: customfield_10050)"), ("api", "cloud or server (default: cloud for *.atlassian.net)"), ("backfill_days", "how far back the first run reads (default 30)"), ("allow_internal", "true to reach a system inside this network (a private address, this machine); never cloud metadata"), ("max_body_mb", "the largest answer read, MiB (default 32, at most 1024)")],
        secrets: &["token"],
        target: "jira_issues",
    },
    Kind {
        name: "oxidized",
        title: "Oxidized configs",
        about: "Device configurations from Oxidized's web API: each node whose backup changed is fetched and kept as a version. Its exec hook can push a node at once.",
        pull: true,
        push: true,
        settings: &[("url", "oxidized-web's base URL"), ("username", "basic auth, if oxidized-web is behind it"), ("max_nodes", "nodes fetched per run (default 200)"), ("allow_internal", "true to reach a system inside this network (a private address, this machine); never cloud metadata"), ("max_body_mb", "the largest answer read, MiB (default 32, at most 1024)")],
        secrets: &["password"],
        target: "device_configs",
    },
    Kind {
        name: "mail",
        title: "Mailbox hook",
        about: "Emails handed over raw (a mail server's pipe, a .forward to curl, an inbound mail route): headers, text, and the BCOP maintenance fields carriers attach (provider, circuits, window, impact).",
        pull: false,
        push: true,
        settings: &[("map", "column -> path, template or value")],
        secrets: &[],
        target: "mail_messages",
    },
];

pub fn kind(name: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.name == name)
}

/// The table a kind makes when its target does not exist.
pub fn default_schema(kind: &str, table: &str) -> String {
    let cols = match kind {
        "prometheus" => "id KEYWORD PRIMARY KEY, ts TIMESTAMP, subject KEYWORD, labels JSON, values_ JSON",
        "servicenow" => "id KEYWORD PRIMARY KEY, number KEYWORD, title TEXT, description TEXT, state KEYWORD, priority KEYWORD, assigned_to KEYWORD, assignment_group KEYWORD, ci KEYWORD, opened TIMESTAMP, updated TIMESTAMP, starts TIMESTAMP, ends TIMESTAMP, data JSON",
        "jira" => "id KEYWORD PRIMARY KEY, project KEYWORD, issuetype KEYWORD, title TEXT, description TEXT, status KEYWORD, priority KEYWORD, assignee KEYWORD, reporter KEYWORD, labels KEYWORD[], created TIMESTAMP, updated TIMESTAMP, resolution KEYWORD, url KEYWORD, fields JSON",
        "oxidized" => "id KEYWORD PRIMARY KEY, device KEYWORD, ip KEYWORD, model KEYWORD, grp KEYWORD, ts TIMESTAMP, version INT, lines INT, sha256 KEYWORD, body TEXT",
        "mail" => "id KEYWORD PRIMARY KEY, sender KEYWORD, subject TEXT, sent TIMESTAMP, received TIMESTAMP, body TEXT, provider KEYWORD, maint_id KEYWORD, circuits KEYWORD[], starts TIMESTAMP, ends TIMESTAMP, impact KEYWORD, status KEYWORD",
        "webhook" | "http_json" => "id KEYWORD PRIMARY KEY, received TIMESTAMP, connector KEYWORD, data JSON, text TEXT",
        _ => "id KEYWORD PRIMARY KEY, data JSON",
    };
    let retain = match kind {
        "prometheus" => " OPTIONS(retain = '7 days', retain_by = 'ts')",
        _ => "",
    };
    format!("CREATE TABLE IF NOT EXISTS {table} ({cols}){retain}")
}

/// The table a connector's own `map` makes when its target does not exist:
/// a column per mapped name, typed by its `"as"` (`int` an INT, `float` a
/// FLOAT, `bool` a BOOL, `time` or `{"now": true}` a TIMESTAMP, `list` a
/// KEYWORD[], `json` a JSON, `hash` a KEYWORD, any other TEXT), `id` (an
/// INT `as` int, else a KEYWORD) its primary key; without an `id`, no
/// primary key, and each record is a new row.
pub fn map_schema(m: &Map<String, Json>, table: &str) -> Result<String, String> {
    let mut cols = Vec::new();
    for (name, spec) in m {
        if name.is_empty() || name.len() > 128 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(format!("map column {name:?} is not a column name: letters, digits and _, not starting with a digit"));
        }
        let ty = match spec {
            Json::Object(o) if o.get("now").and_then(Json::as_bool) == Some(true) && o.get("as").is_none() => "TIMESTAMP",
            Json::Object(o) => match o.get("as").and_then(Json::as_str) {
                Some("int") => "INT",
                Some("float") => "FLOAT",
                Some("bool") => "BOOL",
                Some("time") => "TIMESTAMP",
                Some("list") => "KEYWORD[]",
                Some("json") => "JSON",
                Some("hash") => "KEYWORD",
                _ if name == "id" => "KEYWORD",
                _ => "TEXT",
            },
            Json::Bool(_) => "BOOL",
            Json::Number(n) if n.is_i64() || n.is_u64() => "INT",
            Json::Number(_) => "FLOAT",
            _ if name == "id" => "KEYWORD",
            _ => "TEXT",
        };
        if name == "id" {
            cols.push(format!("id {} PRIMARY KEY", if ty == "INT" { "INT" } else { "KEYWORD" }));
        } else {
            // Quoted: a map may name a column as SQL names a word (`when`).
            cols.push(format!("\"{name}\" {ty}"));
        }
    }
    if cols.is_empty() {
        return Err("the map names no column".into());
    }
    Ok(format!("CREATE TABLE IF NOT EXISTS {table} ({})", cols.join(", ")))
}

/// The map a kind's connector writes its default target with.
fn default_map(c: &Connector) -> Option<Json> {
    Some(match c.kind.as_str() {
        "prometheus" => json!({"id": "$.id", "ts": "$.ts", "subject": "$.subject", "labels": "$.labels", "values_": "$.values"}),
        "servicenow" => json!({"id": "$.number", "number": "$.number", "title": "$.short_description", "description": "$.description", "state": "$.state", "priority": "$.priority", "assigned_to": "$.assigned_to", "assignment_group": "$.assignment_group", "ci": "$.cmdb_ci", "opened": "$.opened_at", "updated": "$.sys_updated_on", "starts": "$.start_date", "ends": "$.end_date", "data": "$"}),
        "jira" => json!({"id": "$.key", "project": "$.project", "issuetype": "$.issuetype", "title": "$.summary", "description": "$.description", "status": "$.status", "priority": "$.priority", "assignee": "$.assignee", "reporter": "$.reporter", "labels": "$.labels", "created": "$.created", "updated": "$.updated", "resolution": "$.resolution", "url": "$.url", "fields": "$.custom"}),
        "oxidized" => json!({"id": "$.id", "device": "$.device", "ip": "$.ip", "model": "$.model", "grp": "$.group", "ts": "$.ts", "version": "$.version", "lines": "$.lines", "sha256": "$.sha256", "body": "$.body"}),
        "mail" => json!({"id": "$.id", "sender": "$.sender", "subject": "$.subject", "sent": "$.date", "received": "$.received", "body": "$.body", "provider": "$.provider", "maint_id": "$.maint_id", "circuits": "$.circuits", "starts": "$.start", "ends": "$.end", "impact": "$.impact", "status": "$.status"}),
        "webhook" if c.config["preset"] == "alertmanager" => json!({"id": "{fingerprint}-{startsAt}", "received": {"now": true}, "connector": {"const": c.name}, "data": "$", "text": "{labels.alertname} {labels.severity} {labels.instance} {annotations.summary} {annotations.description}"}),
        "webhook" | "http_json" => json!({"id": {"as": "hash"}, "received": {"now": true}, "connector": {"const": c.name}, "data": "$", "text": {"as": "text_of"}}),
        _ => return None,
    })
}

/// A preset map into one of the network operations app's tables
/// (`chloedb-netops`), by kind and target.
fn preset_map(c: &Connector) -> Option<Json> {
    let change_status = json!({"new": "planned", "assess": "planned", "authorize": "planned", "scheduled": "approved", "implement": "running", "review": "done", "closed": "done", "canceled": "cancelled", "cancelled": "cancelled", "*": "planned"});
    Some(match (c.kind.as_str(), c.target.as_str()) {
        ("servicenow", "netops_changes") => json!({
            "id": "$.number", "title": "$.short_description", "description": "$.description", "kind": {"path": "$.type", "as": "lower"},
            "risk": {"path": "$.risk", "as": "lower"}, "status": {"path": "$.state", "as": "lower", "values": change_status},
            "window_start": "$.start_date", "window_end": "$.end_date", "owner": "$.assigned_to", "approver": "$.approval_set",
            "devices": {"path": "$.cmdb_ci", "as": "list"}, "rollback_plan": "$.backout_plan", "outcome": {"path": "$.close_code", "as": "lower"},
            "started": "$.work_start", "verified": "$.work_end"
        }),
        ("servicenow", "netops_tickets") => json!({
            "id": "$.number", "title": "$.short_description", "body": "$.description", "status": {"path": "$.state", "as": "lower"},
            "priority": "$.priority", "assignee": "$.assigned_to", "queue": "$.assignment_group", "device": "$.cmdb_ci",
            "customer": "$.company", "opened": "$.opened_at", "updated": "$.sys_updated_on", "resolution": "$.close_notes"
        }),
        ("jira", "netops_changes") => json!({
            "id": "$.key", "title": "$.summary", "description": "$.description", "kind": {"path": "$.issuetype", "as": "lower"},
            "risk": {"path": "$.custom.risk", "as": "lower"}, "status": {"path": "$.status", "as": "lower", "values": json!({"to do": "planned", "open": "planned", "approved": "approved", "in progress": "running", "done": "done", "closed": "done", "cancelled": "cancelled", "*": "planned"})},
            "window_start": "$.custom.window_start", "window_end": "$.custom.window_end", "owner": "$.assignee", "approver": "$.custom.approver",
            "devices": {"path": "$.custom.devices", "as": "list"}, "ticket": "$.key", "rollback_plan": "$.custom.rollback_plan"
        }),
        ("jira", "netops_tickets") => json!({
            "id": "$.key", "title": "$.summary", "body": "$.description", "status": {"path": "$.status", "as": "lower"}, "priority": {"path": "$.priority", "as": "lower"},
            "assignee": "$.assignee", "queue": "$.project", "device": "$.custom.device", "customer": "$.custom.customer",
            "opened": "$.created", "updated": "$.updated", "resolution": "$.resolution"
        }),
        ("oxidized", "netops_configs") => json!({"id": "$.id", "device": "$.device", "version": "$.version", "ts": "$.ts", "author": {"const": "oxidized"}, "lines": "$.lines", "body": "$.body"}),
        ("mail", "netops_maintenance") => json!({
            "id": {"template": "{provider}-{maint_id}", "default": "$.id"}, "provider": "$.provider", "reference": "$.maint_id",
            "circuit": {"path": "$.circuits", "as": "first"}, "window_start": "$.start", "window_end": "$.end", "impact": "$.impact",
            "status": {"path": "$.status", "default": "new"}, "received": "$.received", "subject": "$.subject", "raw": "$.body"
        }),
        ("prometheus", t) if t.starts_with("netops_") => return None,
        _ => return None,
    })
}

/// The map `c` writes with: its own, else a preset for its target, else
/// its kind's for its kind's default table. None: the record is the row.
pub fn map_of(c: &Connector) -> Option<Map<String, Json>> {
    let m = match &c.config["map"] {
        Json::Object(m) if !m.is_empty() => return Some(m.clone()),
        _ => preset_map(c).or_else(|| (c.target == default_target(c)).then(|| default_map(c)).flatten()),
    };
    // A preset's id that fell back to a path: a template of only missing
    // values comes out as "-": keep the map as given.
    m.and_then(|m| match m {
        Json::Object(o) => Some(o),
        _ => None,
    })
}

/// The target a connector of `c`'s kind writes when it names none: the
/// kind's table, or for a webhook preset its own.
pub fn default_target(c: &Connector) -> String {
    match (c.kind.as_str(), c.config["preset"].as_str()) {
        ("webhook", Some("alertmanager")) => "alertmanager_alerts".into(),
        (k, _) => kind(k).map_or("connector_records", |k| k.target).into(),
    }
}

/// What a run reads with: the connector, its cursor, its secrets.
pub struct Ctx<'a> {
    pub c: &'a Connector,
    pub cursor: Option<String>,
    pub secrets: &'a HashMap<String, String>,
    pub http: &'a reqwest::Client,
    /// What its requests may reach (`http` refuses the rest when it
    /// connects; this is checked on each URL first).
    pub reach: &'a reach::Reach,
    pub now_us: i64,
    /// The most records one run reads.
    pub limit: usize,
    /// The most bytes of one answer it reads ([`max_bytes`]).
    pub max_bytes: usize,
}

/// Whether connector `c` may reach this network (`"allow_internal": true`).
pub fn internal(c: &Connector) -> bool {
    c.config.get("allow_internal").and_then(Json::as_bool).unwrap_or(false)
}

/// What connector `c`'s requests may reach.
pub fn reach_of(c: &Connector) -> reach::Reach {
    reach::Reach::new(internal(c))
}

/// The most bytes of an answer connector `c` reads: `max_body_mb` of its
/// config, 32 MiB by default, 1 GiB at most. A pull runs inside the
/// database's process: an answer read whole without a cap could take it
/// down with every table it serves (audit 4, X-N2).
pub fn max_bytes(c: &Connector) -> usize {
    (c.config.get("max_body_mb").and_then(Json::as_u64).unwrap_or(32).clamp(1, 1024) as usize) << 20
}

/// What [`Connector::check`] asks of a config beyond its shape: settings
/// of the right types, and a configured URL that is a URL.
pub fn check_config(c: &Connector) -> Result<(), String> {
    match c.config.get("allow_internal") {
        None | Some(Json::Null) | Some(Json::Bool(_)) => {}
        Some(_) => return Err("allow_internal is true or false".into()),
    }
    match c.config.get("max_body_mb") {
        None | Some(Json::Null) => {}
        Some(v) if v.as_u64().is_some_and(|n| (1..=1024).contains(&n)) => {}
        Some(_) => return Err("max_body_mb is a number of MiB, 1 to 1024".into()),
    }
    for key in ["url", "instance"] {
        if let Some(u) = c.config.get(key).and_then(Json::as_str).filter(|u| !u.trim().is_empty()) {
            // A template's placeholders are not part of its origin.
            let bare = u.replace("{cursor}", "c").replace("{secret:", "{");
            let parsed = reqwest::Url::parse(&bare).map_err(|e| format!("{key} {:?} is not a URL: {e}", strip_query(u)))?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                return Err(format!("{key} is an http or https URL"));
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(format!("{key} names no user or password: give username in the config and the password as a secret"));
            }
        }
    }
    Ok(())
}

/// What a pull read.
#[derive(Debug, Default)]
pub struct Fetched {
    pub records: Vec<Json>,
    /// The cursor to start from next time (none: keep the last one; an
    /// empty one: none).
    pub cursor: Option<String>,
    /// It stopped at the limit: run again soon.
    pub more: bool,
}

impl Ctx<'_> {
    fn s(&self, key: &str) -> Option<&str> {
        self.c.config.get(key).and_then(Json::as_str).filter(|s| !s.trim().is_empty())
    }

    fn need(&self, key: &str) -> Result<&str, String> {
        self.s(key).ok_or_else(|| format!("the {} connector {} needs \"{key}\" in its config", self.c.kind, self.c.name))
    }

    fn u(&self, key: &str, default: u64) -> u64 {
        self.c.config.get(key).and_then(Json::as_u64).unwrap_or(default)
    }

    fn secret(&self, name: &str) -> Option<String> {
        self.secrets.get(name).cloned()
    }

    /// `{secret:NAME}` in `s` replaced by the secret.
    fn with_secrets(&self, s: &str) -> String {
        self.with_secrets_of(s, None)
    }

    /// [`Self::with_secrets`], of the secrets `only` names when it is
    /// given (the others' placeholders are taken out, not expanded).
    fn with_secrets_of(&self, s: &str, only: Option<&[String]>) -> String {
        let mut out = s.to_string();
        let mut from = 0;
        while let Some(i) = out[from..].find("{secret:").map(|i| i + from) {
            let Some(j) = out[i..].find('}') else { break };
            let name = out[i + 8..i + j].to_string();
            let v = match only {
                Some(names) if !names.contains(&name) => String::new(),
                _ => self.secret(&name).unwrap_or_default(),
            };
            out.replace_range(i..i + j + 1, &v);
            from = i + v.len();
        }
        out
    }

    /// The origin the connector's credentials go to: its configured URL's.
    fn origin(&self) -> Option<&str> {
        self.s("url").or_else(|| self.s("instance"))
    }

    /// Whether `url` is on the connector's configured origin.
    fn own_origin(&self, url: &str) -> bool {
        self.origin().is_some_and(|o| same_origin(&o.replace("{cursor}", "c"), url))
    }

    /// A request for `url`, if the connector may reach it, with the
    /// connector's auth and headers when it is on the configured origin
    /// (a next page elsewhere is asked without them).
    fn get(&self, url: &str) -> Result<reqwest::RequestBuilder, String> {
        self.reach.check(url).map_err(|e| format!("GET {}: {e}", strip_query(url)))?;
        let r = self.http.get(url).timeout(Duration::from_secs(60)).header("accept", "application/json");
        if !self.own_origin(url) {
            return Ok(r);
        }
        Ok(self.with_auth(r))
    }

    fn with_auth(&self, mut r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let user = self.s("username");
        match self.s("auth").unwrap_or(if user.is_some() { "basic" } else if self.secrets.contains_key("token") { "bearer" } else { "none" }) {
            "basic" => r = r.basic_auth(user.unwrap_or_default(), self.secret("password").or_else(|| self.secret("token"))),
            "bearer" => {
                if let Some(t) = self.secret("token") {
                    r = r.bearer_auth(t)
                }
            }
            _ => {}
        }
        if let Some(Json::Object(h)) = self.c.config.get("headers") {
            for (k, v) in h {
                r = r.header(k.as_str(), self.with_secrets(&map::text(v)));
            }
        }
        r
    }

    /// The body of `url`'s answer, read up to [`Ctx::max_bytes`], or why
    /// it is not a success. A redirect to another origin is not followed
    /// ([`reach`]): it is answered as an error that says where it led.
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let shown = strip_query(url);
        let mut res = self.get(url)?.send().await.map_err(|e| format!("GET {shown}: {}", http_error(e)))?;
        let status = res.status();
        if status.is_redirection() {
            let to = res.headers().get("location").and_then(|v| v.to_str().ok()).map(|l| strip_query(l).to_string()).unwrap_or_default();
            return Err(format!("GET {shown} answered {status}, a redirect to {to}, which is not followed: a connector follows redirects within its configured origin only, so that its credentials stay there (set its url to where the system is)"));
        }
        let too_big = || format!("GET {shown}: the answer is larger than {} MiB, the most a run reads (\"max_body_mb\" in the connector's config, up to 1024; or ask for less a request)", self.max_bytes >> 20);
        if res.content_length().is_some_and(|n| n > self.max_bytes as u64) {
            return Err(too_big());
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| format!("GET {shown}: {}", http_error(e)))? {
            if body.len() + chunk.len() > self.max_bytes {
                return Err(too_big());
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(format!("GET {shown} answered {status}: {}", String::from_utf8_lossy(&body).chars().take(300).collect::<String>()));
        }
        Ok(body)
    }

    async fn json(&self, url: &str) -> Result<Json, String> {
        let body = self.fetch(url).await?;
        serde_json::from_slice(&body).map_err(|e| format!("GET {}: not JSON ({e})", strip_query(url)))
    }

    async fn text(&self, url: &str) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.fetch(url).await?).into_owned())
    }

    fn base(&self, key: &str) -> Result<String, String> {
        Ok(self.need(key)?.trim_end_matches('/').to_string())
    }
}

/// A URL without its query (which may hold a token), for messages.
fn strip_query(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// `e` for a message. reqwest's own text names the URL it asked, query
/// and all, and a query may hold a secret (`?api_key={secret:token}`):
/// this is the text without the URL, with its causes (what failed to
/// connect, and why).
fn http_error(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut out = e.to_string();
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        let t = c.to_string();
        if !t.is_empty() && !out.contains(&t) {
            out.push_str(": ");
            out.push_str(&t);
        }
        cause = c.source();
    }
    out
}

/// `text` as it may leave a connector (in its state's `last_error`, its
/// runs' rows, a reply, the log), all of which a read key or an operator
/// sees: every URL's query cut off (it may hold a token), and every
/// value of `secrets` (as given, and as it is in a URL) replaced by `***`,
/// wherever a system echoed it.
pub fn redact(text: &str, secrets: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = ["http://", "https://"].iter().filter_map(|p| rest.find(p)).min() {
        out.push_str(&rest[..i]);
        let end = rest[i..].find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '<' | '>' | ',' | ']')).map_or(rest.len(), |j| i + j);
        let url = &rest[i..end];
        out.push_str(strip_query(url));
        if url.contains('?') {
            out.push_str("?...");
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    for v in secrets.values().filter(|v| !v.is_empty()) {
        out = out.replace(v.as_str(), "***");
        let e = enc(v);
        if e != *v {
            out = out.replace(&e, "***");
        }
    }
    out
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One pull of `ctx`'s connector; its error [`redact`]ed.
pub async fn pull(ctx: &Ctx<'_>) -> Result<Fetched, String> {
    pull_unredacted(ctx).await.map_err(|e| redact(&e, ctx.secrets))
}

async fn pull_unredacted(ctx: &Ctx<'_>) -> Result<Fetched, String> {
    match ctx.c.kind.as_str() {
        "prometheus" => prometheus(ctx).await,
        "servicenow" => servicenow(ctx).await,
        "jira" => jira(ctx).await,
        "oxidized" => oxidized(ctx, None).await,
        "http_json" => http_json(ctx).await,
        k => Err(format!("a {k} connector is not pulled: systems push to it (POST /v1/connectors/{}/push)", ctx.c.name)),
    }
}

/// The records of a push to `ctx`'s connector; its error [`redact`]ed.
pub async fn push(ctx: &Ctx<'_>, body: &[u8], content_type: &str) -> Result<Vec<Json>, String> {
    push_unredacted(ctx, body, content_type).await.map_err(|e| redact(&e, ctx.secrets))
}

async fn push_unredacted(ctx: &Ctx<'_>, body: &[u8], content_type: &str) -> Result<Vec<Json>, String> {
    let as_json = || serde_json::from_slice::<Json>(body).map_err(|e| format!("the body is not JSON: {e}"));
    match ctx.c.kind.as_str() {
        "webhook" => {
            let b = as_json()?;
            let path = ctx.s("records").map(str::to_string).or_else(|| (ctx.c.config["preset"] == "alertmanager").then(|| "$.alerts".to_string()));
            Ok(map::records(&b, path.as_deref()))
        }
        "mail" => {
            // Raw mail, or JSON {"raw": "..."} (or a list of them).
            let raws: Vec<Vec<u8>> = if content_type.starts_with("application/json") {
                match as_json()? {
                    Json::Array(a) => a.iter().filter_map(|x| x["raw"].as_str().or(x.as_str()).map(|s| s.as_bytes().to_vec())).collect(),
                    o => vec![o["raw"].as_str().ok_or("a JSON body is {\"raw\": \"<the message>\"}")?.as_bytes().to_vec()],
                }
            } else {
                vec![body.to_vec()]
            };
            Ok(raws.iter().map(|r| super::mail::parse(r, ctx.now_us)).collect())
        }
        "oxidized" => {
            // Oxidized's exec hook: {"node": "...", "group": "..."}, or the
            // config itself as {"node", "text"}.
            let b = as_json()?;
            let node = b["node"].as_str().or(b["name"].as_str()).ok_or("an Oxidized push is {\"node\": \"<name>\", \"group\"?: \"<group>\"}")?.to_string();
            if let Some(text) = b["text"].as_str() {
                return Ok(vec![config_record(&node, &b, text, ctx.now_us)]);
            }
            let f = oxidized(ctx, Some((node, b["group"].as_str().map(str::to_string)))).await?;
            Ok(f.records)
        }
        k => Err(format!("a {k} connector is not pushed to: it pulls on its schedule (POST /v1/admin/connectors/{}/run runs it now)", ctx.c.name)),
    }
}

// ----- prometheus

async fn prometheus(ctx: &Ctx<'_>) -> Result<Fetched, String> {
    let base = ctx.base("url")?;
    let queries = match &ctx.c.config["queries"] {
        Json::Object(q) if !q.is_empty() => q.clone(),
        Json::String(q) => {
            let mut m = Map::new();
            m.insert("value".into(), json!(q));
            m
        }
        _ => return Err(format!("the prometheus connector {} needs \"queries\": {{\"column\": \"PromQL\"}}", ctx.c.name)),
    };
    let step = ctx.u("step_s", 60).max(1);
    let lag = ctx.u("lag_s", 60);
    let now_s = (ctx.now_us / 1_000_000) as u64;
    let end = (now_s.saturating_sub(lag) / step) * step;
    let start = match ctx.cursor.as_deref().and_then(|c| c.parse::<u64>().ok()) {
        Some(c) => c + step,
        None => (end.saturating_sub(ctx.u("backfill_s", 3600)) / step) * step,
    };
    if start > end {
        return Ok(Fetched::default());
    }
    let now_end = end;
    // At most 11,000 points a series per request (Prometheus's limit),
    // and no more steps than a run keeps records.
    let mut end = end.min(start + step * (10_000.min(ctx.limit.max(1)) as u64 - 1));
    let key: Option<Vec<String>> = ctx.c.config["key"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).or_else(|| ctx.s("key").map(|k| k.split(',').map(|x| x.trim().to_string()).collect()));
    let rename: Map<String, Json> = ctx.c.config["rename"].as_object().cloned().unwrap_or_default();
    // (subject labels, ts) -> record
    let mut rows: std::collections::BTreeMap<(String, u64), Map<String, Json>> = std::collections::BTreeMap::new();
    // Records a step: a run keeps ctx.limit records at most, so when the
    // series answered make more, the run ends at an earlier step (and the
    // next goes on from there) rather than keep every point of every
    // series (audit 4, X-N2).
    let mut per_ts: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
    for (col, q) in &queries {
        let q = map::text(q);
        let url = format!("{base}/api/v1/query_range?query={}&start={start}&end={end}&step={step}", enc(&q));
        let j = ctx.json(&url).await?;
        if j["status"] != "success" {
            return Err(format!("Prometheus refused {col}: {}", j["error"].as_str().unwrap_or("no error given")));
        }
        for series in j["data"]["result"].as_array().into_iter().flatten() {
            let labels = series["metric"].as_object().cloned().unwrap_or_default();
            let subject: Vec<(String, String)> = match &key {
                Some(k) => k.iter().map(|l| (l.clone(), labels.get(l).map(map::text).unwrap_or_default())).collect(),
                None => labels.iter().filter(|(k, _)| k.as_str() != "__name__").map(|(k, v)| (k.clone(), map::text(v))).collect(),
            };
            let subject_text = subject.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",");
            for point in series["values"].as_array().into_iter().flatten() {
                let ts = point[0].as_f64().unwrap_or(0.0) as u64;
                if ts > end {
                    continue;
                }
                let Some(v) = point[1].as_str().and_then(|s| s.parse::<f64>().ok()).filter(|x| x.is_finite()) else { continue };
                let rec = rows.entry((subject_text.clone(), ts)).or_insert_with(|| {
                    *per_ts.entry(ts).or_default() += 1;
                    let mut r = Map::new();
                    let ts_text = chloedb::timestamp::format(ts as i64 * 1_000_000);
                    let id_parts: Vec<&str> = subject.iter().map(|(_, v)| v.as_str()).collect();
                    r.insert("id".into(), json!(format!("{}|{ts_text}", id_parts.join("|"))));
                    r.insert("ts".into(), json!(ts_text));
                    r.insert("subject".into(), json!(subject_text));
                    let mut ls = Map::new();
                    for (k, v) in &subject {
                        let name = rename.get(k).and_then(Json::as_str).unwrap_or(k).to_string();
                        r.insert(name, json!(v));
                        ls.insert(k.clone(), json!(v));
                    }
                    r.insert("labels".into(), Json::Object(ls));
                    r.insert("values".into(), json!({}));
                    r
                });
                rec.insert(col.clone(), json!(v));
                if let Some(Json::Object(vs)) = rec.get_mut("values") {
                    vs.insert(col.clone(), json!(v));
                }
            }
            // Twice the limit before cutting, so that each cut is paid for
            // by as many new records as it drops.
            if rows.len() > ctx.limit.saturating_mul(2) {
                end = cut(&mut rows, &mut per_ts, ctx.limit).ok_or_else(|| too_many_series(ctx, per_ts.values().next().copied().unwrap_or(0)))?;
            }
        }
    }
    if rows.len() > ctx.limit {
        end = cut(&mut rows, &mut per_ts, ctx.limit).ok_or_else(|| too_many_series(ctx, per_ts.values().next().copied().unwrap_or(0)))?;
    }
    let records: Vec<Json> = rows.into_values().map(Json::Object).collect();
    let more = end < now_end;
    Ok(Fetched { records, cursor: Some(end.to_string()), more })
}

/// Keeps the records of the earliest steps that fit in `limit`, and
/// answers the last step kept (none: one step alone has more).
fn cut(rows: &mut std::collections::BTreeMap<(String, u64), Map<String, Json>>, per_ts: &mut std::collections::BTreeMap<u64, usize>, limit: usize) -> Option<u64> {
    let mut kept = 0usize;
    let mut last = None;
    for (&ts, &n) in per_ts.iter() {
        if kept + n > limit {
            break;
        }
        kept += n;
        last = Some(ts);
    }
    let last = last?;
    rows.retain(|(_, ts), _| *ts <= last);
    per_ts.retain(|ts, _| *ts <= last);
    Some(last)
}

fn too_many_series(ctx: &Ctx<'_>, n: usize) -> String {
    format!("the queries of the prometheus connector {} answer more series at one step than a run keeps records ({} or more, of {}): aggregate them (sum by, topk) or name fewer", ctx.c.name, n.max(ctx.limit + 1), ctx.limit)
}

// ----- servicenow

/// A ServiceNow record read with `sysparm_display_value=all`, flat: each
/// field its display value (a choice's label, a reference's name), or its
/// value for a time (UTC, where the display is in the user's zone), and
/// `<field>_value` beside it when they differ.
fn sn_flat(r: &Json) -> Json {
    let mut out = Map::new();
    for (k, v) in r.as_object().into_iter().flatten() {
        match v {
            Json::Object(o) if o.contains_key("value") || o.contains_key("display_value") => {
                let value = o.get("value").cloned().unwrap_or(Json::Null);
                let display = o.get("display_value").cloned().unwrap_or(Json::Null);
                let is_time = value.as_str().is_some_and(|s| s.len() == 19 && s.as_bytes().get(10) == Some(&b' ') && map::time_text(s).is_some());
                if is_time {
                    out.insert(k.clone(), value);
                } else {
                    out.insert(k.clone(), if display.is_null() || display == "" { value.clone() } else { display.clone() });
                    if value != display && !value.is_null() && value != "" {
                        out.insert(format!("{k}_value"), value);
                    }
                }
            }
            v => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Json::Object(out)
}

async fn servicenow(ctx: &Ctx<'_>) -> Result<Fetched, String> {
    let base = ctx.base("instance")?;
    let table = ctx.s("table").unwrap_or("change_request");
    if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("{table:?} is not a ServiceNow table's name"));
    }
    // Updated since the cursor, as minutes ago on the instance's clock, so
    // that neither clock's zone matters; a minute more, since rows are
    // upserts and one read twice is the same row.
    let since_us = ctx.cursor.as_deref().and_then(map::time_text).unwrap_or(ctx.now_us - ctx.u("backfill_days", 30) as i64 * 86_400_000_000);
    let minutes = ((ctx.now_us - since_us).max(0) / 60_000_000) + 2;
    let mut q = format!("sys_updated_on>=javascript:gs.minutesAgoStart({minutes})");
    if let Some(extra) = ctx.s("query") {
        q = format!("{extra}^{q}");
    }
    q.push_str("^ORDERBYsys_updated_on");
    let page = 200usize;
    let mut out = Fetched::default();
    let mut cursor_us = since_us;
    let mut offset = 0usize;
    loop {
        let mut url = format!("{base}/api/now/table/{table}?sysparm_query={}&sysparm_display_value=all&sysparm_exclude_reference_link=true&sysparm_limit={page}&sysparm_offset={offset}", enc(&q));
        if let Some(f) = ctx.s("fields") {
            url.push_str(&format!("&sysparm_fields={}", enc(f)));
        }
        let j = ctx.json(&url).await?;
        let batch = j["result"].as_array().cloned().ok_or("ServiceNow answered no \"result\" list")?;
        let n = batch.len();
        for r in batch {
            let flat = sn_flat(&r);
            if let Some(t) = flat["sys_updated_on"].as_str().and_then(map::time_text) {
                cursor_us = cursor_us.max(t);
            }
            out.records.push(flat);
        }
        offset += n;
        if n < page {
            break;
        }
        if out.records.len() >= ctx.limit {
            out.more = true;
            break;
        }
    }
    out.cursor = Some(chloedb::timestamp::format(cursor_us));
    Ok(out)
}

// ----- jira

/// Text of a field in Atlassian's document format (Jira Cloud's v3
/// descriptions and comments), or the field as it is.
fn adf_text(v: &Json) -> Json {
    fn go(v: &Json, out: &mut String) {
        match v {
            Json::Object(o) => {
                if let Some(t) = o.get("text").and_then(Json::as_str) {
                    out.push_str(t);
                }
                if let Some(c) = o.get("content") {
                    go(c, out);
                }
                if matches!(o.get("type").and_then(Json::as_str), Some("paragraph" | "heading" | "listItem" | "hardBreak" | "codeBlock")) {
                    out.push('\n');
                }
            }
            Json::Array(a) => a.iter().for_each(|x| go(x, out)),
            _ => {}
        }
    }
    match v {
        Json::Object(o) if o.get("type").and_then(Json::as_str) == Some("doc") => {
            let mut s = String::new();
            go(v, &mut s);
            json!(s.trim_end())
        }
        v => v.clone(),
    }
}

/// A Jira field's value, plain: a user's name, an option's value, an
/// object's name, a list of those.
fn jira_plain(v: &Json) -> Json {
    match v {
        Json::Object(o) => {
            for k in ["displayName", "value", "name", "key", "emailAddress"] {
                if let Some(x) = o.get(k) {
                    return x.clone();
                }
            }
            if o.get("type").and_then(Json::as_str) == Some("doc") {
                return adf_text(v);
            }
            v.clone()
        }
        Json::Array(a) => Json::Array(a.iter().map(jira_plain).collect()),
        v => v.clone(),
    }
}

fn jira_flat(issue: &Json, base: &str, custom: &Map<String, Json>) -> Json {
    let f = &issue["fields"];
    let mut c = Map::new();
    for (name, id) in custom {
        if let Some(id) = id.as_str() {
            c.insert(name.clone(), jira_plain(&f[id]));
        }
    }
    let key = issue["key"].as_str().unwrap_or_default();
    json!({
        "key": key,
        "id": issue["id"],
        "url": format!("{base}/browse/{key}"),
        "project": f["project"]["key"],
        "issuetype": f["issuetype"]["name"],
        "summary": f["summary"],
        "description": adf_text(&f["description"]),
        "status": f["status"]["name"],
        "status_category": f["status"]["statusCategory"]["name"],
        "priority": f["priority"]["name"],
        "assignee": f["assignee"]["displayName"],
        "reporter": f["reporter"]["displayName"],
        "labels": f["labels"],
        "components": jira_plain(&f["components"]),
        "created": f["created"],
        "updated": f["updated"],
        "resolution": f["resolution"]["name"],
        "resolved": f["resolutiondate"],
        "due": f["duedate"],
        "custom": c,
    })
}

async fn jira(ctx: &Ctx<'_>) -> Result<Fetched, String> {
    let base = ctx.base("url")?;
    let cloud = match ctx.s("api") {
        Some(a) => a == "cloud",
        None => base.contains(".atlassian.net"),
    };
    let since_us = ctx.cursor.as_deref().and_then(map::time_text).unwrap_or(ctx.now_us - ctx.u("backfill_days", 30) as i64 * 86_400_000_000);
    // Relative minutes: Jira reads a date in the account's zone, a "-Nm"
    // on its own clock.
    let minutes = ((ctx.now_us - since_us).max(0) / 60_000_000) + 2;
    let jql = match ctx.s("jql") {
        Some(j) => format!("({j}) AND updated >= \"-{minutes}m\" ORDER BY updated ASC"),
        None => format!("updated >= \"-{minutes}m\" ORDER BY updated ASC"),
    };
    let custom: Map<String, Json> = ctx.c.config["fields"].as_object().cloned().unwrap_or_default();
    let mut fields = "summary,status,priority,issuetype,project,assignee,reporter,labels,components,created,updated,resolution,resolutiondate,duedate,description".to_string();
    for id in custom.values().filter_map(Json::as_str) {
        fields.push(',');
        fields.push_str(id);
    }
    let mut out = Fetched::default();
    let mut cursor_us = since_us;
    let mut token: Option<String> = None;
    let mut start_at = 0u64;
    loop {
        let url = if cloud {
            let mut u = format!("{base}/rest/api/3/search/jql?jql={}&fields={}&maxResults=100", enc(&jql), enc(&fields));
            if let Some(t) = &token {
                u.push_str(&format!("&nextPageToken={}", enc(t)));
            }
            u
        } else {
            format!("{base}/rest/api/2/search?jql={}&fields={}&maxResults=100&startAt={start_at}", enc(&jql), enc(&fields))
        };
        let j = ctx.json(&url).await?;
        let issues = j["issues"].as_array().cloned().ok_or("Jira answered no \"issues\" list")?;
        let n = issues.len();
        for i in &issues {
            let flat = jira_flat(i, &base, &custom);
            if let Some(t) = flat["updated"].as_str().and_then(map::time_text) {
                cursor_us = cursor_us.max(t);
            }
            out.records.push(flat);
        }
        start_at += n as u64;
        let done = if cloud {
            token = j["nextPageToken"].as_str().map(str::to_string);
            token.is_none() || j["isLast"] == true || n == 0
        } else {
            n == 0 || start_at >= j["total"].as_u64().unwrap_or(0)
        };
        if done {
            break;
        }
        if out.records.len() >= ctx.limit {
            out.more = true;
            break;
        }
    }
    out.cursor = Some(chloedb::timestamp::format(cursor_us));
    Ok(out)
}

// ----- oxidized

fn config_record(node: &str, meta: &Json, text: &str, now_us: i64) -> Json {
    let sha = {
        use sha2::Digest;
        let d = sha2::Sha256::digest(text.as_bytes());
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let ts_us = [&meta["mtime"], &meta["last"]["end"], &meta["time"]].into_iter().find_map(map::time).unwrap_or(now_us);
    json!({
        "id": format!("{node}@{}", &sha[..12]),
        "device": node,
        "ip": meta["ip"],
        "model": meta["model"],
        "group": meta["group"],
        "ts": chloedb::timestamp::format(ts_us),
        "version": ts_us / 1_000_000,
        "lines": text.lines().count(),
        "sha256": sha,
        "body": text,
    })
}

async fn oxidized(ctx: &Ctx<'_>, one: Option<(String, Option<String>)>) -> Result<Fetched, String> {
    let base = ctx.base("url")?;
    let fetch_url = |name: &str, group: Option<&str>| match group.filter(|g| !g.is_empty()) {
        Some(g) => format!("{base}/node/fetch/{}/{}", enc(g), enc(name)),
        None => format!("{base}/node/fetch/{}", enc(name)),
    };
    if let Some((name, group)) = one {
        let nodes = ctx.json(&format!("{base}/nodes.json")).await.unwrap_or(json!([]));
        let meta = nodes.as_array().and_then(|a| a.iter().find(|n| n["name"] == name.as_str())).cloned().unwrap_or_else(|| json!({"name": name, "group": group}));
        let group = group.or_else(|| meta["group"].as_str().map(str::to_string));
        let text = ctx.text(&fetch_url(&name, group.as_deref())).await?;
        return Ok(Fetched { records: vec![config_record(&name, &meta, &text, ctx.now_us)], cursor: None, more: false });
    }
    let nodes = ctx.json(&format!("{base}/nodes.json")).await?;
    let nodes = nodes.as_array().cloned().ok_or("Oxidized's /nodes.json answered no list")?;
    // The cursor: each node's last backup time we fetched.
    let mut seen: Map<String, Json> = ctx.cursor.as_deref().and_then(|c| serde_json::from_str(c).ok()).unwrap_or_default();
    let max = ctx.u("max_nodes", 200) as usize;
    let mut out = Fetched::default();
    for n in &nodes {
        let Some(name) = n["name"].as_str() else { continue };
        let stamp = [&n["mtime"], &n["last"]["end"], &n["time"]].into_iter().find(|v| !v.is_null()).map(map::text).unwrap_or_default();
        if stamp.is_empty() || seen.get(name).and_then(Json::as_str) == Some(stamp.as_str()) {
            continue;
        }
        if n["last"]["status"].as_str().is_some_and(|s| s != "success" && s != "no_connection") && n["mtime"].is_null() {
            continue;
        }
        if out.records.len() >= max {
            out.more = true;
            break;
        }
        let text = ctx.text(&fetch_url(name, n["group"].as_str())).await?;
        out.records.push(config_record(name, n, &text, ctx.now_us));
        seen.insert(name.to_string(), json!(stamp));
    }
    out.cursor = Some(Json::Object(seen).to_string());
    Ok(out)
}

// ----- http_json

/// Whether `v` is past `b`: by number when both are numbers, else as text.
fn higher(b: Option<&Json>, v: &Json) -> bool {
    match (b, v) {
        (_, Json::Null) => false,
        (None, _) => true,
        (Some(b), v) => match (map::number(b), map::number(v)) {
            (Some(x), Some(y)) => y > x,
            _ => map::text(v) > map::text(b),
        },
    }
}

/// The names of the secrets `{secret:NAME}` in `s` names.
fn secret_names(s: &str) -> Vec<String> {
    s.match_indices("{secret:").filter_map(|(i, _)| s[i + 8..].find('}').map(|j| s[i + 8..i + 8 + j].to_string())).collect()
}

/// `url` as it may be kept in the connector's state (which a read key
/// sees): each secret's value, as given or as it is in a URL, back to its
/// `{secret:NAME}`, which [`Ctx::with_secrets`] puts back when it is asked.
fn with_placeholders(url: &str, secrets: &HashMap<String, String>) -> String {
    let mut out = url.to_string();
    let mut by_len: Vec<_> = secrets.iter().filter(|(_, v)| !v.is_empty()).collect();
    by_len.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for (name, v) in by_len {
        out = out.replace(&enc(v), &format!("{{secret:{name}}}")).replace(v.as_str(), &format!("{{secret:{name}}}"));
    }
    out
}

/// An `http_json` pull. Without a cursor that moves the URL on (a
/// `{cursor}` in the URL and a `cursor` path in the records), a run that
/// stops at its limit keeps the next page's URL in its cursor as
/// `{"next": URL, "cursor": C}`, and the next run goes on from there
/// rather than from the first page again; the run that reads the last page
/// clears it.
async fn http_json(ctx: &Ctx<'_>) -> Result<Fetched, String> {
    let url0 = ctx.need("url")?;
    let kept = ctx.cursor.clone().filter(|c| !c.is_empty());
    let resume: Option<(String, Option<String>)> = kept.as_deref().and_then(|k| serde_json::from_str::<Json>(k).ok()).and_then(|j| {
        let next = j.get("next")?.as_str()?.to_string();
        Some((next, j.get("cursor").and_then(Json::as_str).map(str::to_string)))
    });
    let cursor = match &resume {
        Some((_, c)) => c.clone(),
        None => kept,
    };
    let start_cursor = cursor.clone().or_else(|| ctx.s("start").map(str::to_string)).unwrap_or_default();
    let mut url = match &resume {
        Some((next, _)) => next.clone(),
        None => url0.replace("{cursor}", &enc(&start_cursor)),
    };
    let records_path = ctx.s("records").map(str::to_string);
    let mut out = Fetched::default();
    let mut best: Option<Json> = None;
    let mut left: Option<String> = None;
    // A URL from the upstream's answer (a next page, or one kept from
    // it) has only the secrets the configured URL names expanded, and only
    // on the configured origin: an upstream that answers
    // `"next": "https://elsewhere/?k={secret:password}"` gets none.
    let named = secret_names(url0);
    for page in 0..1000 {
        let asked = match page == 0 && resume.is_none() {
            true => ctx.with_secrets(&url),
            false if ctx.own_origin(&url) => ctx.with_secrets_of(&url, Some(&named)),
            false => ctx.with_secrets_of(&url, Some(&[])),
        };
        let j = ctx.json(&asked).await?;
        let batch = map::records(&j, records_path.as_deref());
        if let Some(cp) = ctx.s("cursor") {
            for r in &batch {
                let v = map::path(r, cp);
                if higher(best.as_ref(), &v) {
                    best = Some(v);
                }
            }
        }
        out.records.extend(batch);
        let Some(n) = ctx.s("next").map(|p| map::text(&map::path(&j, p))).filter(|n| !n.is_empty()) else { break };
        let n = if n.starts_with("http://") || n.starts_with("https://") {
            n
        } else if n.starts_with('/') {
            let origin: String = url0.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
            format!("{origin}{n}")
        } else {
            n
        };
        if out.records.len() >= ctx.limit || page == 999 {
            left = Some(n);
            break;
        }
        url = n;
    }
    // The highest cursor value read so far in this pass.
    let pass_best = match (best, cursor.as_ref().map(|c| Json::String(c.clone()))) {
        (Some(b), Some(c)) => Some(if higher(Some(&c), &b) { b } else { c }),
        (b, c) => b.or(c),
    };
    let pass_best = pass_best.map(|b| map::text(&b));
    let moves_on = url0.contains("{cursor}") && ctx.s("cursor").is_some() && pass_best.is_some() && pass_best != cursor;
    match left {
        // The next run starts from the new cursor.
        Some(_) if moves_on && resume.is_none() => {
            out.more = true;
            out.cursor = pass_best;
        }
        // The next run goes on from the next page.
        Some(next) => {
            out.more = true;
            out.cursor = Some(json!({"next": with_placeholders(&next, ctx.secrets), "cursor": pass_best}).to_string());
        }
        // The last page: the pass is over.
        None => out.cursor = Some(pass_best.unwrap_or_default()),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servicenow_records_flatten_to_labels_and_utc_times() {
        let r = json!({"number": {"value": "CHG0001", "display_value": "CHG0001"}, "state": {"value": "-2", "display_value": "Scheduled"}, "start_date": {"value": "2026-11-20 06:00:00", "display_value": "11/20/2026 01:00:00 AM"}, "assigned_to": {"value": "6816f79c", "display_value": "Beth Anglin"}});
        let f = sn_flat(&r);
        assert_eq!(f["number"], "CHG0001");
        assert_eq!(f["state"], "Scheduled");
        assert_eq!(f["state_value"], "-2");
        assert_eq!(f["start_date"], "2026-11-20 06:00:00");
        assert_eq!(f["assigned_to"], "Beth Anglin");
    }

    #[test]
    fn jira_issues_flatten_with_their_document_text_and_custom_fields() {
        let i = json!({"key": "NET-7", "id": "10007", "fields": {"summary": "Upgrade cr1", "status": {"name": "In Progress", "statusCategory": {"name": "In Progress"}}, "issuetype": {"name": "Change"}, "project": {"key": "NET"}, "assignee": {"displayName": "Ana"}, "description": {"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Reload "}, {"type": "text", "text": "cr1-nyc"}]}]}, "customfield_1": "2026-11-20T06:00:00.000+0000", "customfield_2": [{"value": "cr1-nyc"}, {"value": "cr2-nyc"}]}});
        let mut custom = Map::new();
        custom.insert("window_start".into(), json!("customfield_1"));
        custom.insert("devices".into(), json!("customfield_2"));
        let f = jira_flat(&i, "https://x.atlassian.net", &custom);
        assert_eq!(f["description"], "Reload cr1-nyc");
        assert_eq!(f["status"], "In Progress");
        assert_eq!(f["url"], "https://x.atlassian.net/browse/NET-7");
        assert_eq!(f["custom"]["devices"], json!(["cr1-nyc", "cr2-nyc"]));
    }

    fn connector(kind: &str, config: Json) -> Connector {
        Connector { name: "t".into(), kind: kind.into(), target: "t".into(), enabled: true, every_s: 60, config, push_key: None, note: String::new(), created_ms: 0, updated_ms: 0, updated_by: String::new(), invalid: None }
    }

    #[test]
    fn redact_cuts_queries_and_masks_secrets_wherever_they_are() {
        let secrets: HashMap<String, String> = [("token".to_string(), "SUPER secret/1".to_string())].into();
        let e = "GET http://h:1/x: error sending request for url (http://h:1/x?api_key=SUPER%20secret%2F1&b=2), and SUPER secret/1 echoed, see https://h/y?q=1.";
        let r = redact(e, &secrets);
        assert!(!r.contains("SUPER") && !r.contains("api_key"), "{r}");
        assert_eq!(r, "GET http://h:1/x: error sending request for url (http://h:1/x?...), and *** echoed, see https://h/y?...");
    }

    #[test]
    fn next_page_urls_keep_secrets_as_placeholders() {
        let secrets: HashMap<String, String> = [("token".to_string(), "s3 cr/t".to_string())].into();
        assert_eq!(with_placeholders("http://h/x?key=s3%20cr%2Ft&page=2", &secrets), "http://h/x?key={secret:token}&page=2");
        assert_eq!(with_placeholders("http://h/s3 cr/t", &secrets), "http://h/{secret:token}");
    }

    #[test]
    fn a_map_makes_a_table_of_its_columns() {
        let m = json!({"id": "$.id", "title": "$.title", "n": {"path": "$.n", "as": "int"}, "when": {"path": "$.when", "as": "time"}, "seen": {"now": true}, "tags": {"path": "$.tags", "as": "list"}, "x": 1.5});
        let ddl = map_schema(m.as_object().unwrap(), "items_t").unwrap();
        for col in ["id KEYWORD PRIMARY KEY", "\"title\" TEXT", "\"n\" INT", "\"when\" TIMESTAMP", "\"seen\" TIMESTAMP", "\"tags\" KEYWORD[]", "\"x\" FLOAT"] {
            assert!(ddl.contains(col), "{col}: {ddl}");
        }
        assert!(map_schema(json!({"bad name": "$.a"}).as_object().unwrap(), "t").is_err());
        assert!(!map_schema(json!({"a": "$.a"}).as_object().unwrap(), "t").unwrap().contains("PRIMARY KEY"));
    }

    /// A source of `n` records, ten a page, each page naming the next.
    async fn pages(n: usize) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::extract::Query;
        let gets = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let g = gets.clone();
        let app = axum::Router::new().route(
            "/items",
            axum::routing::get(move |Query(q): Query<HashMap<String, String>>| {
                let g = g.clone();
                async move {
                    g.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let p: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(0);
                    let items: Vec<Json> = (p * 10..((p + 1) * 10).min(n)).map(|i| json!({"id": i})).collect();
                    let next = ((p + 1) * 10 < n).then(|| format!("/items?page={}&key={}", p + 1, q.get("key").cloned().unwrap_or_default()));
                    axum::Json(json!({"items": items, "next": next}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, gets)
    }

    #[tokio::test]
    async fn a_pull_without_a_cursor_goes_on_from_the_page_it_stopped_at() {
        let (up, gets) = pages(45).await;
        let c = connector("http_json", json!({"url": format!("{up}/items?key={{secret:token}}"), "records": "$.items", "next": "$.next"}));
        let secrets: HashMap<String, String> = [("token".to_string(), "k3y".to_string())].into();
        let http = reqwest::Client::new();
        let mut cursor = None;
        let mut ids = Vec::new();
        let mut runs = 0;
        loop {
            let ctx = Ctx { c: &c, cursor: cursor.clone(), secrets: &secrets, http: &http, reach: &reach::Reach::new(true), now_us: 0, limit: 20, max_bytes: 1 << 20 };
            let f = pull(&ctx).await.unwrap();
            runs += 1;
            ids.extend(f.records.iter().map(|r| r["id"].as_u64().unwrap()));
            if let Some(cur) = f.cursor {
                assert!(!cur.contains("k3y"), "a secret is not kept in the state: {cur}");
                cursor = Some(cur).filter(|c| !c.is_empty());
            }
            if !f.more {
                break;
            }
            assert!(runs < 5, "it does not start over: {ids:?}");
        }
        assert_eq!(ids, (0..45).collect::<Vec<_>>(), "each record once, in three runs");
        assert_eq!((runs, cursor), (3, None), "the pass is over: the next run starts from the first page");
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn a_failed_request_names_no_secret_and_no_query() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let c = connector("http_json", json!({"url": format!("http://127.0.0.1:{port}/x?api_key={{secret:token}}"), "records": "$.items"}));
        let secrets: HashMap<String, String> = [("token".to_string(), "SUPERSECRET123".to_string())].into();
        let http = reqwest::Client::new();
        let ctx = Ctx { c: &c, cursor: None, secrets: &secrets, http: &http, reach: &reach::Reach::new(true), now_us: 0, limit: 20, max_bytes: 1 << 20 };
        let e = pull(&ctx).await.unwrap_err();
        assert!(!e.contains("SUPERSECRET123") && !e.contains("api_key"), "{e}");
        assert!(e.starts_with(&format!("GET http://127.0.0.1:{port}/x: error sending request")), "{e}");
    }

    /// A Prometheus answering `series` series, a point each step.
    async fn prometheus_of(series: usize) -> String {
        use axum::extract::Query;
        let app = axum::Router::new().route(
            "/api/v1/query_range",
            axum::routing::get(move |Query(q): Query<HashMap<String, String>>| async move {
                let n = |k: &str| q.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let (start, end, step) = (n("start"), n("end"), n("step").max(1));
                let result: Vec<Json> = (0..series).map(|i| json!({"metric": {"device": format!("r{i}")}, "values": (start..=end).step_by(step as usize).map(|t| json!([t, "1"])).collect::<Vec<_>>()})).collect();
                axum::Json(json!({"status": "success", "data": {"resultType": "matrix", "result": result}}))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn a_prometheus_pull_keeps_a_runs_records_at_most_and_goes_on_from_its_last_step() {
        let up = prometheus_of(6).await;
        let c = connector("prometheus", json!({"url": up, "queries": {"a": "a", "b": "b"}, "key": ["device"], "step_s": 60, "lag_s": 0, "backfill_s": 3600}));
        let secrets = HashMap::new();
        let http = reqwest::Client::new();
        let now_us = 1_800_000_000 * 1_000_000;
        let mut cursor = None;
        let mut steps = std::collections::BTreeSet::new();
        for run in 0.. {
            let ctx = Ctx { c: &c, cursor: cursor.clone(), secrets: &secrets, http: &http, reach: &reach::Reach::new(true), now_us, limit: 20, max_bytes: 1 << 20 };
            let f = pull(&ctx).await.unwrap();
            assert!(f.records.len() <= 20, "run {run}: {} records", f.records.len());
            for r in &f.records {
                assert!(r["a"] == 1.0 && r["b"] == 1.0, "each kept step has every query: {r}");
                assert!(steps.insert((r["device"].as_str().unwrap().to_string(), r["ts"].as_str().unwrap().to_string())), "each record once");
            }
            cursor = f.cursor;
            if !f.more {
                break;
            }
            assert!(run < 40, "it goes on");
        }
        assert_eq!(steps.len(), 6 * 61, "every step of every series, an hour and its end");
        // More series at one step than a run keeps: said, not kept.
        let up = prometheus_of(25).await;
        let c = connector("prometheus", json!({"url": up, "queries": {"a": "a"}, "key": ["device"], "lag_s": 0}));
        let ctx = Ctx { c: &c, cursor: None, secrets: &secrets, http: &http, reach: &reach::Reach::new(true), now_us, limit: 20, max_bytes: 1 << 20 };
        assert!(pull(&ctx).await.unwrap_err().contains("more series at one step"));
    }

    #[test]
    fn every_kind_has_a_default_table() {
        for k in KINDS {
            assert!(default_schema(k.name, k.target).starts_with("CREATE TABLE IF NOT EXISTS"));
        }
    }
}
