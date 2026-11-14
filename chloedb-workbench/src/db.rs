//! The state database: `<data-dir>/workbench.chloedb`, a ChloeDB file with
//! two tables, `agents` (a row per session, replaced on every change) and
//! `events` (a row per hook call, spawn, input and exit). The server is its
//! only writer.

use std::path::Path;
use std::sync::Arc;

use chloedb::db::{Db, SqlResult};
use chloedb::query::Params;
use chloedb::storage::{DbOptions, MergerOptions};
use chloedb::Value;
use serde_json::{json, Value as Json};

pub type Result<T> = std::result::Result<T, String>;

const AGENTS: &str = "CREATE TABLE IF NOT EXISTS agents (
  id KEYWORD PRIMARY KEY,
  name KEYWORD,
  repo KEYWORD,
  cwd KEYWORD,
  branch KEYWORD,
  task TEXT,
  parent KEYWORD,
  depth INT,
  state KEYWORD,
  state_since TIMESTAMP,
  waiting_for TEXT,
  last_event TEXT,
  session_id KEYWORD,
  pid INT,
  exit_code INT,
  model KEYWORD,
  created TIMESTAMP,
  ended TIMESTAMP,
  base KEYWORD,
  started_ms INT,
  preview_url KEYWORD,
  goal KEYWORD,
  outcome KEYWORD,
  narration TEXT,
  narration_ts TIMESTAMP
)";

/// The columns the decision trail added, for a database made before them.
const AGENTS_ADDED: &str = "ALTER TABLE agents ADD COLUMN IF NOT EXISTS base KEYWORD, ADD COLUMN IF NOT EXISTS started_ms INT";
/// The columns the levels added (Preview), for a database made before them.
const AGENTS_ADDED_PREVIEW: &str = "ALTER TABLE agents ADD COLUMN IF NOT EXISTS preview_url KEYWORD";

/// Small key/value state: allow-lists, applied suggestions.
const MARKS: &str = "CREATE TABLE IF NOT EXISTS marks (
  \"key\" KEYWORD PRIMARY KEY,
  value TEXT,
  ts TIMESTAMP
)";

/// The columns the levels added (goal, outcome, narration).
const AGENTS_ADDED_LEVELS: &str = "ALTER TABLE agents ADD COLUMN IF NOT EXISTS goal KEYWORD, ADD COLUMN IF NOT EXISTS outcome KEYWORD, ADD COLUMN IF NOT EXISTS narration TEXT, ADD COLUMN IF NOT EXISTS narration_ts TIMESTAMP";

/// The levels' tables (SPEC.md, "Levels"): goals and their outcomes,
/// questions to you and the rules made from your answers, what each agent
/// has been told, and `seen` (small key/value state by `id`: `last_seen`;
/// `key` is a reserved word of ChloeDB's SQL).
const LEVEL_TABLES: [&str; 6] = [
    "CREATE TABLE IF NOT EXISTS goals (id KEYWORD PRIMARY KEY, repo KEYWORD, text TEXT, state KEYWORD, lead KEYWORD, created TIMESTAMP, done TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS outcomes (id KEYWORD PRIMARY KEY, goal KEYWORD, title TEXT, detail TEXT, state KEYWORD, agent KEYWORD, ord INT, after KEYWORD, progress INT, created TIMESTAMP, updated TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS questions (id KEYWORD PRIMARY KEY, agent KEYWORD, outcome KEYWORD, repo KEYWORD, kind KEYWORD, text TEXT, context TEXT, options TEXT, recommended INT, precedent TEXT, state KEYWORD, answer TEXT, answered_by KEYWORD, rule KEYWORD, created TIMESTAMP, answered TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS rules (id KEYWORD PRIMARY KEY, repo KEYWORD, text TEXT, question KEYWORD, ctx_id KEYWORD, active BOOL, created TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS deliveries (id KEYWORD PRIMARY KEY, agent KEYWORD, item KEYWORD, via KEYWORD, ts TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS seen (id KEYWORD PRIMARY KEY, value TEXT, ts TIMESTAMP)",
];

/// Health's tables (SPEC.md, "Fourth build", rows 4 and 5): alert rules as
/// rows (SQL over these tables), what they fired on, the incidents the
/// firings group into, every test an agent ran, every file an agent edited,
/// and the tokens each agent's transcript says it used.
const HEALTH_TABLES: [&str; 6] = [
    "CREATE TABLE IF NOT EXISTS alert_rules (id KEYWORD PRIMARY KEY, repo KEYWORD, name TEXT, description TEXT, sql_text TEXT OPTIONS(indexed = false), cmp KEYWORD, threshold FLOAT, window_s INT, severity KEYWORD, action KEYWORD, builtin BOOL, enabled BOOL, created TIMESTAMP, updated TIMESTAMP, last_eval TIMESTAMP, last_ms FLOAT, last_rows INT, last_error TEXT, firing INT)",
    "CREATE TABLE IF NOT EXISTS alert_firings (id KEYWORD PRIMARY KEY, rule KEYWORD, repo KEYWORD, entity KEYWORD, agent KEYWORD, file KEYWORD, detail TEXT, value FLOAT, summary TEXT, state KEYWORD, started TIMESTAMP, last_seen TIMESTAMP, ended TIMESTAMP, incident KEYWORD, misses INT, agents TEXT)",
    "CREATE TABLE IF NOT EXISTS incidents (id KEYWORD PRIMARY KEY, repo KEYWORD, title TEXT, severity KEYWORD, state KEYWORD, agents TEXT, files TEXT, rules TEXT, actions TEXT, needs_you BOOL, started TIMESTAMP, updated TIMESTAMP, resolved TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS test_runs (id KEYWORD PRIMARY KEY, run KEYWORD, repo KEYWORD, agent KEYWORD, \"commit\" KEYWORD, tree KEYWORD, runner KEYWORD, command TEXT, test KEYWORD, outcome KEYWORD, passed BOOL, ms INT, own_ms BOOL, run_ms INT, exit_code INT, error TEXT, use_id KEYWORD, ts TIMESTAMP, seq INT)",
    "CREATE TABLE IF NOT EXISTS file_edits (id KEYWORD PRIMARY KEY, repo KEYWORD, agent KEYWORD, file KEYWORD, tool KEYWORD, ts TIMESTAMP, seq INT)",
    "CREATE TABLE IF NOT EXISTS token_use (id KEYWORD PRIMARY KEY, repo KEYWORD, agent KEYWORD, input INT, output INT, cache_read INT, ts TIMESTAMP)",
];

/// Time and guard (SPEC.md, "Fourth build", rows 8, 9 and 6): the state
/// database's snapshots (src/timetravel.rs), guarded commands' change
/// requests (src/guarded.rs), and each finished task's features
/// (src/routing.rs).
const TIME_GUARD_TABLES: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS time_snapshots (id KEYWORD PRIMARY KEY, ts TIMESTAMP, reason KEYWORD, detail TEXT, txn INT, heads TEXT)",
    "CREATE TABLE IF NOT EXISTS change_requests (id KEYWORD PRIMARY KEY, agent KEYWORD, repo KEYWORD, cwd KEYWORD, branch KEYWORD, command TEXT, why TEXT, rule KEYWORD, risk KEYWORD, risk_why TEXT, diff TEXT, precedent TEXT, state KEYWORD, output TEXT, exit_code INT, ms INT, decided_by KEYWORD, reason TEXT, told KEYWORD, created TIMESTAMP, decided TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS task_features (id KEYWORD PRIMARY KEY, repo KEYWORD, name KEYWORD, task TEXT, files TEXT, model KEYWORD, input INT, output INT, cache_read INT, cost_usd FLOAT, minutes FLOAT, redo INT, done BOOL, outcome KEYWORD, exit_code INT, ended TIMESTAMP)",
];

/// Search's tables (SPEC.md, "Fourth build", rows 1 and 10): the files and
/// chunks of each repository's index (a tree per checkout: `main`, or an
/// agent's id), the searches agents made, and the docs of their libraries.
const SEARCH_TABLES: [&str; 6] = [
    "CREATE TABLE IF NOT EXISTS code_files (id KEYWORD PRIMARY KEY, repo KEYWORD, tree KEYWORD, path KEYWORD, lang KEYWORD, sig KEYWORD, chunks INT, lines INT, deleted BOOL, indexed TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS code_chunks (id KEYWORD PRIMARY KEY, repo KEYWORD, tree KEYWORD, path KEYWORD, lang KEYWORD, symbol KEYWORD, kind KEYWORD, start_line INT, end_line INT, text TEXT OPTIONS(analyzer = 'simple'), terms TEXT OPTIONS(analyzer = 'simple'), head TEXT OPTIONS(analyzer = 'simple'))",
    "CREATE TABLE IF NOT EXISTS search_log (id KEYWORD PRIMARY KEY, ts TIMESTAMP, repo KEYWORD, agent KEYWORD, kind KEYWORD, q TEXT, hits INT, tokens INT, cites TEXT)",
    "CREATE TABLE IF NOT EXISTS doc_libs (id KEYWORD PRIMARY KEY, repo KEYWORD, lib KEYWORD, name KEYWORD, version KEYWORD, ecosystem KEYWORD, manifest KEYWORD, url KEYWORD, state KEYWORD, pages INT, error TEXT, crawled TIMESTAMP, updated TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS doc_pages (id KEYWORD PRIMARY KEY, lib KEYWORD, repo KEYWORD, url KEYWORD, title TEXT, text TEXT, words INT, depth INT, fetched TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS doc_cites (id KEYWORD PRIMARY KEY, ts TIMESTAMP, repo KEYWORD, agent KEYWORD, lib KEYWORD, url KEYWORD, title TEXT, q TEXT)",
];

/// The Preview lab's tables (SPEC.md, "Fourth build", use cases 2 and 3):
/// persona runs and their findings, runtime errors, and the agents' edits
/// that the errors are joined to (`ASOF JOIN` on `ts_ms`).
const PREVIEW_LAB_TABLES: [&str; 4] = [
    "CREATE TABLE IF NOT EXISTS swarm_runs (id KEYWORD PRIMARY KEY, agent KEYWORD, repo KEYWORD, persona TEXT, task TEXT, model KEYWORD, adapter KEYWORD, started_by KEYWORD, state KEYWORD, outcome KEYWORD, answer TEXT, steps TEXT, step_count INT, max_steps INT, error TEXT, cost_usd FLOAT, created TIMESTAMP, ended TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS swarm_findings (id KEYWORD PRIMARY KEY, run KEYWORD, agent KEYWORD, repo KEYWORD, kind KEYWORD, severity INT, title TEXT, detail TEXT, step INT, url TEXT, state KEYWORD, question KEYWORD, steer KEYWORD, created TIMESTAMP, updated TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS runtime_errors (id KEYWORD PRIMARY KEY, agent KEYWORD, repo KEYWORD, cwd KEYWORD, source KEYWORD, level KEYWORD, message TEXT, stack TEXT, file KEYWORD, line INT, col INT, url TEXT, status INT, service KEYWORD, fingerprint KEYWORD, hits INT, first_ms INT, last_ms INT, state KEYWORD)",
    "CREATE TABLE IF NOT EXISTS runtime_edits (id KEYWORD PRIMARY KEY, seq INT, ts_ms INT, agent KEYWORD, repo KEYWORD, cwd KEYWORD, file KEYWORD, line INT, tool KEYWORD)",
];

/// Fifth build: what the boxes complete (src/complete.rs), one row per
/// suggestion of a repository (`*` for the global ones), found by `PREFIX`
/// over `pfx` and by BM25 over `body`.
const COMPLETE_ITEMS: &str = "CREATE TABLE IF NOT EXISTS complete_items (id KEYWORD PRIMARY KEY, repo KEYWORD, kind KEYWORD, pfx KEYWORD[], label TEXT OPTIONS(indexed = false), ins TEXT OPTIONS(indexed = false), detail TEXT OPTIONS(indexed = false), refj TEXT OPTIONS(indexed = false), body TEXT, weight FLOAT, grp KEYWORD)";

/// Fifth build: what each user's boxes showed, what they picked and passed
/// over, and what they wrote (kind `typed`): the habits ranking learns from.
const COMPLETE_EVENTS: &str = "CREATE TABLE IF NOT EXISTS complete_events (id KEYWORD PRIMARY KEY, \"user\" KEYWORD, repo KEYWORD, field KEYWORD, agent KEYWORD, kind KEYWORD, label KEYWORD, ctx TEXT OPTIONS(indexed = false), text TEXT, accepted BOOL, rank INT, source KEYWORD, ts TIMESTAMP)";

const EVENTS: &str = "CREATE TABLE IF NOT EXISTS events (
  id KEYWORD PRIMARY KEY,
  seq INT,
  ts TIMESTAMP,
  agent KEYWORD,
  kind KEYWORD,
  tool KEYWORD,
  summary TEXT,
  data TEXT
)";

const AGENT_COLS: &str = "id, name, repo, cwd, branch, task, parent, depth, state, state_since, waiting_for, last_event, session_id, pid, exit_code, model, created, ended, base, started_ms, preview_url, goal, outcome, narration, narration_ts";
const EVENT_COLS: &str = "id, seq, ts, agent, kind, tool, summary, data";

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    chloedb::timestamp::now() / 1000
}

/// ISO 8601 text of a time in milliseconds.
pub fn iso(ms: i64) -> String {
    chloedb::timestamp::format(ms * 1000)
}

/// An agent's row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub task: Option<String>,
    pub parent: Option<String>,
    pub depth: i64,
    pub state: String,
    pub state_since: i64,
    pub waiting_for: Option<String>,
    pub last_event: Option<String>,
    pub session_id: Option<String>,
    pub pid: Option<i64>,
    pub exit_code: Option<i64>,
    pub model: Option<String>,
    pub created: i64,
    pub ended: Option<i64>,
    /// The commit its worktree started from.
    pub base: Option<String>,
    /// When its process started (milliseconds since the Unix epoch): the
    /// zero of its terminal recording.
    pub started_ms: Option<i64>,
    /// Where its app runs (Preview): `http://127.0.0.1:<port>` or
    /// `http://localhost:<port>`.
    pub preview_url: Option<String>,
    /// The goal it works toward, and its outcome (SPEC.md, "Levels").
    pub goal: Option<String>,
    pub outcome: Option<String>,
    /// One live sentence of what it does, and when it was set (ms).
    pub narration: Option<String>,
    pub narration_ts: Option<i64>,
}

impl Agent {
    /// The row as JSON (times as ISO 8601 text), plus `running` and
    /// `children`, as the API answers it.
    pub fn to_json(&self, running: bool, children: Vec<String>) -> Json {
        json!({
            "id": self.id,
            "name": self.name,
            "repo": self.repo,
            "cwd": self.cwd,
            "branch": self.branch,
            "task": self.task,
            "parent": self.parent,
            "depth": self.depth,
            "state": self.state,
            "state_since": iso(self.state_since),
            "waiting_for": self.waiting_for,
            "last_event": self.last_event,
            "session_id": self.session_id,
            "pid": self.pid,
            "exit_code": self.exit_code,
            "model": self.model,
            "created": iso(self.created),
            "ended": self.ended.map(iso),
            "base": self.base,
            "started_ms": self.started_ms,
            "preview_url": self.preview_url,
            "goal": self.goal,
            "outcome": self.outcome,
            "narration": self.narration,
            "narration_ts": self.narration_ts.map(iso),
            "running": running,
            "children": children,
        })
    }
}

/// An event's row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    pub id: String,
    pub seq: i64,
    pub ts: i64,
    pub agent: String,
    pub kind: String,
    pub tool: Option<String>,
    pub summary: Option<String>,
    pub data: Option<String>,
}

impl Event {
    pub fn to_json(&self, with_data: bool) -> Json {
        let mut v = json!({
            "id": self.id,
            "seq": self.seq,
            "ts": iso(self.ts),
            "agent": self.agent,
            "kind": self.kind,
            "tool": self.tool,
            "summary": self.summary,
        });
        if with_data {
            v["data"] = json!(self.data);
        }
        v
    }
}

pub struct StateDb {
    db: Arc<Db>,
}

fn e(err: chloedb::Error) -> String {
    err.to_string()
}

pub fn opt_str(v: &Option<String>) -> Value {
    v.as_ref().map_or(Value::Null, |s| Value::Str(s.clone()))
}

pub fn opt_int(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::Int)
}

pub fn ts(ms: i64) -> Value {
    Value::Timestamp(ms * 1000)
}

pub fn get_str(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Str(s) => Some(s.clone()),
        Value::List(l) => l.first().and_then(get_str),
        Value::Int(i) => Some(i.to_string()),
        _ => None,
    }
}

pub fn get_int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Timestamp(t) => Some(*t),
        Value::Float(f) => Some(*f as i64),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

pub fn get_ms(v: &Value) -> Option<i64> {
    match v {
        Value::Timestamp(t) => Some(t.div_euclid(1000)),
        Value::Int(t) => Some(t.div_euclid(1000)),
        Value::Str(s) => chloedb::timestamp::parse(s).map(|t| t.div_euclid(1000)),
        _ => None,
    }
}

impl StateDb {
    /// Opens `<dir>/workbench.chloedb`, creating it and its tables if needed.
    pub fn open(dir: &Path) -> Result<StateDb> {
        std::fs::create_dir_all(dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
        let path = dir.join("workbench.chloedb");
        let opts = DbOptions {
            // A crash of this process loses nothing without it; a power
            // failure may lose the last moments of a session's events.
            sync: false,
            ..DbOptions::default()
        };
        let db = if path.exists() { Db::open(&path, opts) } else { Db::create_empty(&path, opts) }.map_err(|err| format!("cannot open {}: {err}", path.display()))?;
        db.sql(AGENTS, &Params::new()).map_err(e)?;
        db.sql(AGENTS_ADDED, &Params::new()).map_err(e)?;
        db.sql(AGENTS_ADDED_LEVELS, &Params::new()).map_err(e)?;
        db.sql(AGENTS_ADDED_PREVIEW, &Params::new()).map_err(e)?;
        db.sql(MARKS, &Params::new()).map_err(e)?;
        for t in LEVEL_TABLES {
            db.sql(t, &Params::new()).map_err(e)?;
        }
        for t in TIME_GUARD_TABLES {
            db.sql(t, &Params::new()).map_err(e)?;
        }
        for t in SEARCH_TABLES {
            db.sql(t, &Params::new()).map_err(e)?;
        }
        db.sql(EVENTS, &Params::new()).map_err(e)?;
        for t in HEALTH_TABLES {
            db.sql(t, &Params::new()).map_err(e)?;
        }
        for t in PREVIEW_LAB_TABLES {
            db.sql(t, &Params::new()).map_err(e)?;
        }
        // Fifth build: Hotspots (src/hotspots.rs).
        db.sql(crate::hotspots::TABLE, &Params::new()).map_err(e)?;
        // Sixth build: each test command's summary, and test runs' waits (src/testruns.rs).
        db.sql(crate::testruns::SUMMARIES, &Params::new()).map_err(e)?;
        db.sql(crate::testruns::ADDED, &Params::new()).map_err(e)?;
        db.sql(COMPLETE_ITEMS, &Params::new()).map_err(e)?;
        db.sql(COMPLETE_EVENTS, &Params::new()).map_err(e)?;
        // Sixth build: repositories opened without an agent (src/repos.rs).
        db.sql(crate::repos::TABLE, &Params::new()).map_err(e)?;
        // The rewrite: Workbench's grants (src/rulesuggest.rs).
        db.sql(crate::rulesuggest::TABLE, &Params::new()).map_err(e)?;
        // The journeys: a repository whose own checkout agents may work in (src/repos.rs).
        db.sql(crate::repos::TRUST_TABLE, &Params::new()).map_err(e)?;
        let db = Arc::new(db);
        db.spawn_flusher();
        db.spawn_merger(MergerOptions { threads: 1, ..MergerOptions::default() });
        Ok(StateDb { db })
    }

    /// Runs a statement of the levels' tables: its rows, or none for a write.
    pub fn query(&self, sql: &str, params: &Params) -> Result<Vec<Vec<Value>>> {
        self.rows(sql, params)
    }

    /// Runs a statement and answers its columns' names with its rows (Health's
    /// rules, whose columns are whatever their SQL names).
    pub fn query_cols(&self, sql: &str, params: &Params) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        match self.db.sql(sql, params).map_err(e)? {
            SqlResult::Rows(r) => Ok((r.columns, r.rows)),
            SqlResult::Written(_) => Ok((Vec::new(), Vec::new())),
        }
    }

    fn rows(&self, sql: &str, params: &Params) -> Result<Vec<Vec<Value>>> {
        match self.db.sql(sql, params).map_err(e)? {
            SqlResult::Rows(r) => Ok(r.rows),
            SqlResult::Written(_) => Ok(Vec::new()),
        }
    }

    /// Writes an agent's row, replacing the one of its id.
    pub fn put_agent(&self, a: &Agent) -> Result<()> {
        let p = Params::new()
            .set("id", Value::Str(a.id.clone()))
            .set("name", Value::Str(a.name.clone()))
            .set("repo", Value::Str(a.repo.clone()))
            .set("cwd", Value::Str(a.cwd.clone()))
            .set("branch", opt_str(&a.branch))
            .set("task", opt_str(&a.task))
            .set("parent", opt_str(&a.parent))
            .set("depth", Value::Int(a.depth))
            .set("state", Value::Str(a.state.clone()))
            .set("state_since", ts(a.state_since))
            .set("waiting_for", opt_str(&a.waiting_for))
            .set("last_event", opt_str(&a.last_event))
            .set("session_id", opt_str(&a.session_id))
            .set("pid", opt_int(a.pid))
            .set("exit_code", opt_int(a.exit_code))
            .set("model", opt_str(&a.model))
            .set("created", ts(a.created))
            .set("ended", a.ended.map_or(Value::Null, ts))
            .set("base", opt_str(&a.base))
            .set("started_ms", opt_int(a.started_ms))
            .set("preview_url", opt_str(&a.preview_url))
            .set("goal", opt_str(&a.goal))
            .set("outcome", opt_str(&a.outcome))
            .set("narration", opt_str(&a.narration))
            .set("narration_ts", a.narration_ts.map_or(Value::Null, ts));
        let names: Vec<String> = AGENT_COLS.split(", ").map(|c| format!(":{c}")).collect();
        let sql = format!("INSERT OR REPLACE INTO agents ({AGENT_COLS}) VALUES ({})", names.join(", "));
        self.db.sql(&sql, &p).map_err(e)?;
        Ok(())
    }

    /// Every agent, oldest first.
    pub fn agents(&self) -> Result<Vec<Agent>> {
        let rows = self.rows(&format!("SELECT {AGENT_COLS} FROM agents ORDER BY created ASC LIMIT 1000000"), &Params::new())?;
        Ok(rows
            .iter()
            .map(|r| Agent {
                id: get_str(&r[0]).unwrap_or_default(),
                name: get_str(&r[1]).unwrap_or_default(),
                repo: get_str(&r[2]).unwrap_or_default(),
                cwd: get_str(&r[3]).unwrap_or_default(),
                branch: get_str(&r[4]),
                task: get_str(&r[5]),
                parent: get_str(&r[6]),
                depth: get_int(&r[7]).unwrap_or(0),
                state: get_str(&r[8]).unwrap_or_default(),
                state_since: get_ms(&r[9]).unwrap_or(0),
                waiting_for: get_str(&r[10]),
                last_event: get_str(&r[11]),
                session_id: get_str(&r[12]),
                pid: get_int(&r[13]),
                exit_code: get_int(&r[14]),
                model: get_str(&r[15]),
                created: get_ms(&r[16]).unwrap_or(0),
                ended: get_ms(&r[17]),
                base: r.get(18).and_then(get_str),
                started_ms: r.get(19).and_then(get_int),
                preview_url: r.get(20).and_then(get_str),
                goal: r.get(21).and_then(get_str),
                outcome: r.get(22).and_then(get_str),
                narration: r.get(23).and_then(get_str),
                narration_ts: r.get(24).and_then(get_ms),
            })
            .collect())
    }

    /// Writes an event's row.
    pub fn add_event(&self, ev: &Event) -> Result<()> {
        let p = Params::new()
            .set("id", Value::Str(ev.id.clone()))
            .set("seq", Value::Int(ev.seq))
            .set("ts", ts(ev.ts))
            .set("agent", Value::Str(ev.agent.clone()))
            .set("kind", Value::Str(ev.kind.clone()))
            .set("tool", opt_str(&ev.tool))
            .set("summary", opt_str(&ev.summary))
            .set("data", opt_str(&ev.data));
        let sql = format!("INSERT OR REPLACE INTO events ({EVENT_COLS}) VALUES (:id, :seq, :ts, :agent, :kind, :tool, :summary, :data)");
        self.db.sql(&sql, &p).map_err(e)?;
        Ok(())
    }

    /// The events of `agent` after `after`, oldest first.
    pub fn events(&self, agent: &str, after: i64, limit: usize) -> Result<Vec<Event>> {
        let p = Params::new().set("agent", Value::Str(agent.to_string())).set("after", Value::Int(after));
        let rows = self.rows(&format!("SELECT {EVENT_COLS} FROM events WHERE agent = :agent AND seq > :after ORDER BY seq ASC LIMIT {limit}"), &p)?;
        Ok(rows.iter().map(event_of).collect())
    }

    /// Every event of `agent`, oldest first.
    pub fn all_events(&self, agent: &str) -> Result<Vec<Event>> {
        self.events(agent, i64::MIN, 1_000_000)
    }

    /// The highest event number written, or 0.
    pub fn last_seq(&self) -> Result<i64> {
        let rows = self.rows("SELECT seq FROM events ORDER BY seq DESC LIMIT 1", &Params::new())?;
        Ok(rows.first().and_then(|r| get_int(&r[0])).unwrap_or(0))
    }

    /// A mark's value and when it was set (milliseconds).
    pub fn mark(&self, key: &str) -> Result<Option<(String, i64)>> {
        let rows = self.rows("SELECT value, ts FROM marks WHERE \"key\" = :key LIMIT 1", &Params::new().set("key", Value::Str(key.to_string())))?;
        Ok(rows.first().and_then(|r| Some((get_str(&r[0])?, get_ms(&r[1]).unwrap_or(0)))))
    }

    /// Sets a mark, replacing its value.
    pub fn set_mark(&self, key: &str, value: &str) -> Result<()> {
        let p = Params::new().set("key", Value::Str(key.to_string())).set("value", Value::Str(value.to_string())).set("ts", ts(now_ms()));
        self.db.sql("INSERT OR REPLACE INTO marks (\"key\", value, ts) VALUES (:key, :value, :ts)", &p).map_err(e)?;
        Ok(())
    }

    /// Opens a snapshot (a checkpoint [`StateDb::checkpoint`] made) to read
    /// it and nothing else: no tables are made, no flusher or merger runs.
    pub fn open_read_only(path: &Path) -> Result<StateDb> {
        let mut opts = DbOptions::default();
        opts.read_only = true;
        let db = Db::open(path, opts).map_err(|err| format!("cannot open {}: {err}", path.display()))?;
        Ok(StateDb { db: Arc::new(db) })
    }

    /// Copies the database to `dest` (a new directory) in ChloeDB's
    /// directory layout, hard-linking the segments `link_from` (an earlier
    /// copy) holds unchanged; answers the commit the copy holds.
    pub fn checkpoint(&self, dest: &Path, link_from: Option<&Path>) -> Result<u64> {
        use chloedb::storage::{CheckpointOptions, Layout};
        let opts = |l: Option<&Path>| CheckpointOptions { layout: Layout::Directory, link_from: l.map(Path::to_path_buf) };
        let stats = match self.db.storage().checkpoint(dest, &opts(link_from)) {
            // The copy it would link from does not read: a full copy.
            Err(_) if link_from.is_some() && !dest.exists() => self.db.storage().checkpoint(dest, &opts(None)),
            r => r,
        }
        .map_err(e)?;
        Ok(if stats.source_txn > 0 { stats.source_txn } else { stats.txn })
    }

    /// Writes what is in memory to the file.
    pub fn close(&self) {
        let _ = self.db.close();
    }
}

fn event_of(r: &Vec<Value>) -> Event {
    Event {
        id: get_str(&r[0]).unwrap_or_default(),
        seq: get_int(&r[1]).unwrap_or(0),
        ts: get_ms(&r[2]).unwrap_or(0),
        agent: get_str(&r[3]).unwrap_or_default(),
        kind: get_str(&r[4]).unwrap_or_default(),
        tool: get_str(&r[5]),
        summary: get_str(&r[6]),
        data: get_str(&r[7]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_from_before_the_decision_trail_opens_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Db::create_empty(&dir.path().join("workbench.chloedb"), DbOptions::default()).unwrap();
            let old = AGENTS.split("  base KEYWORD").next().unwrap().trim_end().trim_end_matches(',').to_string() + "\n)";
            db.sql(&old, &Params::new()).unwrap();
            db.sql("INSERT INTO agents (id, name, repo, cwd, depth, state, created) VALUES ('a1', 'old', '/r', '/r', 0, 'exited', '2026-10-01T12:30:00Z')", &Params::new()).unwrap();
            db.close().unwrap();
        }
        let s = StateDb::open(dir.path()).unwrap();
        let mut a = s.agents().unwrap().pop().unwrap();
        assert_eq!((a.id.as_str(), a.base.as_deref(), a.started_ms), ("a1", None, None));
        assert_eq!((a.goal.as_deref(), a.outcome.as_deref(), a.narration.as_deref(), a.narration_ts), (None, None, None, None));
        a.base = Some("abc".into());
        a.started_ms = Some(42);
        a.preview_url = Some("http://127.0.0.1:5173".into());
        s.put_agent(&a).unwrap();
        let b = s.agents().unwrap().pop().unwrap();
        assert_eq!((b.base.as_deref(), b.started_ms), (Some("abc"), Some(42)));
        assert_eq!(b.preview_url.as_deref(), Some("http://127.0.0.1:5173"));
        // Marks.
        assert_eq!(s.mark("allow:/r").unwrap(), None);
        s.set_mark("allow:/r", "[\"Bash(ls:*)\"]").unwrap();
        s.set_mark("allow:/r", "[\"Bash(cargo test:*)\"]").unwrap();
        let (v, ts) = s.mark("allow:/r").unwrap().unwrap();
        assert_eq!(v, "[\"Bash(cargo test:*)\"]");
        assert!(ts > 0);
        a.goal = Some("g1".into());
        a.outcome = Some("o1".into());
        a.narration = Some("Editing src/a.rs".into());
        a.narration_ts = Some(1_700_000_000_000);
        s.put_agent(&a).unwrap();
        let b = s.agents().unwrap().pop().unwrap();
        assert_eq!(b.preview_url.as_deref(), Some("http://127.0.0.1:5173"));
        assert_eq!((b.goal.as_deref(), b.outcome.as_deref(), b.narration.as_deref(), b.narration_ts), (Some("g1"), Some("o1"), Some("Editing src/a.rs"), Some(1_700_000_000_000)));
        // The levels' tables are there.
        for t in ["goals", "outcomes", "questions", "rules", "deliveries", "seen", "time_snapshots", "change_requests", "task_features"] {
            s.query(&format!("SELECT * FROM {t} LIMIT 1"), &Params::new()).unwrap();
        }
        s.close();
    }
}
