//! The Model Context Protocol (MCP): agents such as Claude Code drive
//! Strata through tools, over `POST /v1/mcp` (the protocol's Streamable
//! HTTP transport, answering JSON) or over stdio with
//! `chloedb-server mcp --url ...`, which sends each message to that route.
//!
//! A tool is one of the server's own routes: `query` is `POST /v1/query`,
//! `execute` is `POST /v1/sql`, `write` is `POST /v1/write`, and so on. A
//! call is sent to the router in this process with the caller's
//! credential, so it passes the same checks as the route called directly:
//! the key's scope (a read key's `execute` of an INSERT is refused), the
//! contain rules, the audit log and the query log, where it is the
//! client's app `mcp`. Two tools need no route: `check_sql` binds a
//! statement against the tables without running it, and `search_docs`
//! finds sections of the query language reference compiled into the
//! binary.
//!
//! The server keeps no session: every message is answered on its own, so
//! any coordinator of a cluster answers any message. It offers tools and
//! resources (the query language and the Build an app guide), and no
//! stream of its own (`GET /v1/mcp` answers 405).

use std::net::SocketAddr;
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value as Json};

use crate::service::Service;

/// Protocol versions this server speaks, newest first. `initialize`
/// answers the client's version when it is one of these, else the newest.
pub const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Rows a `query` answers unless it asks for more ([`MAX_ROWS`] at most):
/// a model reads every row it is given.
pub const DEFAULT_ROWS: usize = 100;
pub const MAX_ROWS: usize = 2000;
/// Characters of a tool's answer, past which it is cut.
pub const MAX_TEXT: usize = 200_000;
/// Bytes of a route's answer read.
const MAX_BODY: usize = 64 << 20;

/// The query language's reference and the Build an app guide, as the
/// documentation site has them.
pub const QUERY_LANGUAGE: &str = include_str!("../../docs/chloedb/query-language.md");
pub const BUILD_AN_APP: &str = include_str!("../../docs/chloedb/build-an-app.md");

const INSTRUCTIONS: &str = "ChloeDB is a search database with SQL: full text (BM25), vectors, graphs and structured fields, in tables. \
Start with list_tables: it gives each table's CREATE TABLE statement. \
ChloeDB SQL is its own dialect, not Postgres: MATCH(field, 'words') filters by text, ORDER BY bm25(field, 'words') DESC ranks by it, \
similarity(vector_field, :p) ranks by meaning (bind :p in params as {\"embed\": \"some text\", \"field\": \"emb\"}), and search_docs finds the syntax of anything else. \
query runs SELECT only; execute runs INSERT, UPDATE, DELETE, CREATE, ALTER and DROP; write sends rows as JSON; import loads many rows at once (a CSV file's text, JSON lines or objects) into a table. \
check_sql parses and binds a statement against the tables without running it, and explain shows how a SELECT runs. \
list_feedback and get_feedback read what people asked of the command center, with screenshots; update_feedback takes an item and marks it done. \
The key you connect with decides what you may change (whoami): read, write, or admin for tables. \
With an admin, ops or propose key there are tools to operate the server too: health, cluster, alerts, events, queries, logs and audit read it; \
plan_* and dry_run show what a change would do without making it; contain, cancel_query, silence_alert and ack_alert act at once, for a while, with a reason; \
anything bigger is propose_change, which a person approves. runbook finds what to do for an alert or a problem.";

mod ops_tools;

/// What a tool's call does.
enum Act {
    /// A route of this server: its method, path (with its query string)
    /// and body; then what is kept of its answer.
    Route { method: Method, path: String, body: Option<Json>, shape: Shape },
    /// `check_sql`: bound here.
    CheckSql(String),
    /// `search_docs`: found here.
    SearchDocs { query: String, limit: usize },
    /// An operation of tier 1 and up ([`crate::ops`]): a route sent with
    /// the reason the agent gave, which the audit log keeps.
    Action { method: Method, path: String, body: Option<Json>, reason: String },
    /// A runbook, found here ([`crate::runbooks`]).
    Runbook { query: String },
    /// `import`: rows as text, sent to `POST /v1/import` with their
    /// content type.
    Import { path: String, content_type: &'static str, data: Vec<u8>, create: Option<String> },
}

/// What a tool keeps of a route's answer.
#[derive(Clone, Copy)]
enum Shape {
    All,
    /// `/v1/tables`, without each table's schema in full.
    Tables,
    /// Rows, at most this many.
    Rows(usize),
    /// A feedback item: its `shot`, a `data:` URL, answered as an image.
    Shot,
}

/// Why a tool's call failed: arguments the tool does not take (a protocol
/// error), or the call's own failure, which the model reads and may fix.
enum Failed {
    Arguments(String),
    Call(String),
}

fn s<'a>(args: &'a Json, k: &str) -> Result<&'a str, Failed> {
    args.get(k).and_then(|v| v.as_str()).filter(|v| !v.trim().is_empty()).ok_or_else(|| Failed::Arguments(format!("the arguments need a string {k:?}")))
}

fn opt_s<'a>(args: &'a Json, k: &str) -> Option<&'a str> {
    args.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty())
}

/// The body's members that are present, of those named.
fn pick(args: &Json, keys: &[&str]) -> Json {
    let mut o = serde_json::Map::new();
    for k in keys {
        if let Some(v) = args.get(*k).filter(|v| !v.is_null()) {
            o.insert(k.to_string(), v.clone());
        }
    }
    Json::Object(o)
}

/// Percent-encoding of a query string's value.
pub fn encode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn route(method: Method, path: impl Into<String>, body: Option<Json>, shape: Shape) -> Act {
    Act::Route { method, path: path.into(), body, shape }
}

/// The tools: name, title, description, input schema, annotations, and
/// what a call does.
struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: fn() -> Json,
    read_only: bool,
    destructive: bool,
    act: fn(&Json) -> Result<Act, Failed>,
}

const PARAMS: &str = "Values of the statement's :name parameters, by name. A vector parameter may be {\"embed\": \"text\", \"field\": \"emb\"} (the text embedded by that field's model) or {\"vector_of\": \"<key>\", \"field\": \"emb\"} (that row's vector).";

fn sql_schema(more: Json) -> Json {
    let mut props = json!({
        "sql": {"type": "string", "description": "One ChloeDB SQL statement."},
        "params": {"type": "object", "description": PARAMS},
    });
    if let (Some(p), Some(m)) = (props.as_object_mut(), more.as_object()) {
        p.extend(m.clone());
    }
    json!({"type": "object", "properties": props, "required": ["sql"]})
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "list_tables",
        title: "List tables",
        description: "Every table: its CREATE TABLE statement (fields, types, options), primary key, shard key, rank profiles, and the models of its embedded and derived fields. Call it first.",
        schema: || json!({"type": "object", "properties": {}}),
        read_only: true,
        destructive: false,
        act: |_| Ok(route(Method::GET, "/v1/tables", None, Shape::Tables)),
    },
    Tool {
        name: "describe_table",
        title: "Describe a table",
        description: "One table's schema in full: every field with its type and options, as JSON and as CREATE TABLE.",
        schema: || json!({"type": "object", "properties": {"table": {"type": "string"}}, "required": ["table"]}),
        read_only: true,
        destructive: false,
        act: |a| Ok(route(Method::GET, format!("/v1/schema?table={}", encode(s(a, "table")?)), None, Shape::All)),
    },
    Tool {
        name: "query",
        title: "Query",
        description: "Run one SELECT (read-only) and get its columns and rows. Search with MATCH(text_field, 'words') to filter and ORDER BY bm25(text_field, 'words') DESC or similarity(vector_field, :p) DESC to rank; GROUP BY for facets; WITH, joins, window functions and UNION as in SQL. Answers at most max_rows rows (default 100), and says when it cut them.",
        schema: || sql_schema(json!({
            "max_rows": {"type": "integer", "minimum": 1, "maximum": MAX_ROWS, "default": DEFAULT_ROWS},
            "timeout_ms": {"type": "integer", "minimum": 1}
        })),
        read_only: true,
        destructive: false,
        act: |a| {
            s(a, "sql")?;
            let rows = a.get("max_rows").and_then(|v| v.as_u64()).map_or(DEFAULT_ROWS, |n| (n as usize).clamp(1, MAX_ROWS));
            Ok(route(Method::POST, "/v1/query", Some(pick(a, &["sql", "params", "timeout_ms"])), Shape::Rows(rows)))
        },
    },
    Tool {
        name: "explain",
        title: "Explain a query",
        description: "How a SELECT runs: the bound query, its plan and the executor's steps; with hits, also the top rows with their score() and the features that made it.",
        schema: || sql_schema(json!({"hits": {"type": "integer", "minimum": 1, "maximum": 50}})),
        read_only: true,
        destructive: false,
        act: |a| {
            s(a, "sql")?;
            Ok(route(Method::POST, "/v1/explain", Some(pick(a, &["sql", "params", "hits"])), Shape::All))
        },
    },
    Tool {
        name: "check_sql",
        title: "Check a statement",
        description: "Parse and bind one statement against the tables without running it: what it would do (a CREATE TABLE's schema, the table an INSERT, UPDATE or DELETE writes, the changes of an ALTER), or why ChloeDB refuses it. Use it before execute. A SELECT is bound when it runs: check it with explain.",
        schema: || json!({"type": "object", "properties": {"sql": {"type": "string"}}, "required": ["sql"]}),
        read_only: true,
        destructive: false,
        act: |a| Ok(Act::CheckSql(s(a, "sql")?.to_string())),
    },
    Tool {
        name: "execute",
        title: "Execute a statement",
        description: "Run one statement that changes data or tables: INSERT, UPDATE, DELETE (a write key), CREATE TABLE, ALTER TABLE, DROP TABLE, CREATE RANK PROFILE (an admin key). Answers how many rows it wrote. A SELECT here answers rows too, but use query for those.",
        schema: || sql_schema(json!({})),
        read_only: false,
        destructive: true,
        act: |a| {
            s(a, "sql")?;
            Ok(route(Method::POST, "/v1/sql", Some(pick(a, &["sql", "params"])), Shape::Rows(MAX_ROWS)))
        },
    },
    Tool {
        name: "write",
        title: "Write rows",
        description: "Apply operations in one transaction: {\"op\": \"upsert\" | \"insert\" | \"upsert_if_newer\", \"table\": \"t\", \"doc\": {...}} writes a row (upsert replaces any row with its key; insert fails if one exists), {\"op\": \"delete\", \"table\": \"t\", \"key\": \"k\"} deletes one, {\"op\": \"set\", \"table\": \"t\", \"key\": \"k\", \"field\": \"f\", \"value\": v} sets one field, {\"op\": \"add\", ..., \"field\": \"stock\", \"value\": -1} adds to a number. Fields left out of a doc are NULL (or their DEFAULT); embedded vectors are computed by the server. Needs a write key.",
        schema: || json!({"type": "object", "properties": {"ops": {"type": "array", "minItems": 1, "items": {"type": "object", "properties": {
            "op": {"type": "string", "enum": ["upsert", "insert", "upsert_if_newer", "delete", "set", "add"]},
            "table": {"type": "string"},
            "doc": {"type": "object"},
            "key": {},
            "field": {"type": "string"},
            "value": {}
        }, "required": ["op"]}}}, "required": ["ops"]}),
        read_only: false,
        destructive: true,
        act: |a| match a.get("ops") {
            Some(Json::Array(o)) if !o.is_empty() => Ok(route(Method::POST, "/v1/write", Some(json!({"ops": o})), Shape::All)),
            _ => Err(Failed::Arguments("the arguments need an array \"ops\" of one operation or more".into())),
        },
    },
    Tool {
        name: "import",
        title: "Import rows",
        description: "Load many rows into a table at once: CSV or TSV text (the first line names the columns), JSON lines, a JSON array of objects as text in data, or the objects themselves in rows. The table must exist (list_tables shows it), or with create: true an admin key makes it first from the rows: types from the values (whole numbers INT, numbers FLOAT, true/false BOOL, ISO dates TIMESTAMP, longer text of several words TEXT, other text KEYWORD), the primary key a column named id (or key); the answer shows the CREATE TABLE it ran. Columns the table has not are skipped and named in the answer; a row with the key of one there replaces it. Answers how many rows it wrote. Needs a write key. For a few rows, write works too.",
        schema: || json!({"type": "object", "properties": {
            "table": {"type": "string"},
            "format": {"type": "string", "enum": ["csv", "tsv", "jsonl", "json"], "description": "the format of data (default csv); rows are JSON"},
            "data": {"type": "string", "description": "the rows as text, such as a CSV file's contents"},
            "rows": {"type": "array", "items": {"type": "object"}, "description": "the rows as objects, in place of data"},
            "empty": {"type": "string", "enum": ["default", "null", "text"], "description": "what an empty CSV cell is: the field's DEFAULT (the default), NULL, or empty text"},
            "strict": {"type": "boolean", "description": "refuse a column the table does not have, rather than skip it"},
            "create": {"type": "boolean", "description": "make the table from the rows when there is none (needs an admin key)"},
            "key": {"type": "string", "description": "with create: the column that is the primary key (default a column named id; \"\" for none)"}
        }, "required": ["table"]}),
        read_only: false,
        destructive: true,
        act: import_act,
    },
    Tool {
        name: "get_rows",
        title: "Get rows by key",
        description: "Rows by their primary keys, in the order given (null for a key with no live row), without SQL.",
        schema: || json!({"type": "object", "properties": {
            "keys": {"type": "array", "minItems": 1, "items": {}},
            "table": {"type": "string"},
            "columns": {"type": "array", "items": {"type": "string"}}
        }, "required": ["keys"]}),
        read_only: true,
        destructive: false,
        act: |a| match a.get("keys") {
            Some(Json::Array(k)) if !k.is_empty() => Ok(route(Method::POST, "/v1/kv/get", Some(pick(a, &["keys", "table", "columns"])), Shape::All)),
            _ => Err(Failed::Arguments("the arguments need an array \"keys\"".into())),
        },
    },
    Tool {
        name: "json_paths",
        title: "Paths of a JSON field",
        description: "The keys a JSON field's objects hold: each path, how many rows have it, its types, and its commonest values, under path if given and among the rows a where condition matches. Use it to learn what to filter on (attrs.color = 'red').",
        schema: || json!({"type": "object", "properties": {
            "table": {"type": "string"}, "field": {"type": "string"},
            "path": {"type": "string"}, "where": {"type": "string", "description": "A WHERE condition over the table's columns."}
        }, "required": ["table", "field"]}),
        read_only: true,
        destructive: false,
        act: |a| {
            let mut path = format!("/v1/tables/{}/paths?field={}", encode(s(a, "table")?), encode(s(a, "field")?));
            for k in ["path", "where"] {
                if let Some(v) = opt_s(a, k) {
                    path.push_str(&format!("&{k}={}", encode(v)));
                }
            }
            Ok(route(Method::GET, path, None, Shape::All))
        },
    },
    Tool {
        name: "list_models",
        title: "List models",
        description: "The embedding models a table's VECTOR field may name (local and hosted), their dimensions, and whether each would run on this server.",
        schema: || json!({"type": "object", "properties": {}}),
        read_only: true,
        destructive: false,
        act: |_| Ok(route(Method::GET, "/v1/models", None, Shape::All)),
    },
    Tool {
        name: "judge",
        title: "Judge results",
        description: "Grade how well each row answers a search query, 0 (not at all) to 3 (perfectly), by a Claude model reading the row's text fields: a relevance check of a ranking before and after a change. Needs a write key and the server's Anthropic key.",
        schema: || json!({"type": "object", "properties": {
            "query": {"type": "string"},
            "keys": {"type": "array", "minItems": 1, "items": {}, "description": "The primary keys of the rows to grade."},
            "table": {"type": "string"},
            "fields": {"type": "array", "items": {"type": "string"}},
            "model": {"type": "string"}
        }, "required": ["query", "keys"]}),
        read_only: false,
        destructive: false,
        act: |a| {
            s(a, "query")?;
            Ok(route(Method::POST, "/v1/judge", Some(pick(a, &["query", "keys", "table", "fields", "model"])), Shape::All))
        },
    },
    Tool {
        name: "search_docs",
        title: "Search the docs",
        description: "Sections of ChloeDB's query language reference and its Build an app guide that match the words given: the syntax of a function, an option, a type or a statement.",
        schema: || json!({"type": "object", "properties": {
            "query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 8, "default": 3}
        }, "required": ["query"]}),
        read_only: true,
        destructive: false,
        act: |a| Ok(Act::SearchDocs { query: s(a, "query")?.to_string(), limit: a.get("limit").and_then(|v| v.as_u64()).map_or(3, |n| (n as usize).clamp(1, 8)) }),
    },
    Tool {
        name: "list_feedback",
        title: "List feedback",
        description: "What people asked of the command center (the server's web UI): each item's id, kind (bug, change, idea, question), status (new, triaged, planned, doing, done, wontfix), priority (0 now to 3 some day), title, note, the view it is about, and its numbered marks on the screenshot as text, newest first. status is open (the default: new, triaged, planned and doing), all, or a list such as new,planned. It is table cc_feedback, which query reads too.",
        schema: || json!({"type": "object", "properties": {
            "status": {"type": "string", "default": "open"},
            "kind": {"type": "string", "enum": ["bug", "change", "idea", "question"]},
            "q": {"type": "string", "description": "Words of the note, the title or the marks."},
            "assignee": {"type": "string"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100}
        }}),
        read_only: true,
        destructive: false,
        act: |a| {
            let mut q = Vec::new();
            for k in ["status", "kind", "q", "assignee"] {
                if let Some(v) = opt_s(a, k) {
                    q.push(format!("{k}={}", encode(v)));
                }
            }
            if let Some(n) = a.get("limit").and_then(|v| v.as_u64()) {
                q.push(format!("limit={n}"));
            }
            Ok(route(Method::GET, format!("/v1/feedback?{}", q.join("&")), None, Shape::All))
        },
    },
    Tool {
        name: "get_feedback",
        title: "Get a feedback item",
        description: "One feedback item in full: its note, its marks (each with its place on the screenshot in CSS pixels, what it says, and the element under it: selector, text, card), the page's context (window size, theme, errors on the page), its triage, and the screenshot with the marks drawn on it, as an image.",
        schema: || json!({"type": "object", "properties": {
            "id": {"type": "string"},
            "shot": {"type": "boolean", "default": true, "description": "Answer the screenshot as an image."}
        }, "required": ["id"]}),
        read_only: true,
        destructive: false,
        act: |a| {
            let id = encode(s(a, "id")?);
            Ok(match a.get("shot").and_then(|v| v.as_bool()).unwrap_or(true) {
                true => route(Method::GET, format!("/v1/feedback/{id}?shot=1"), None, Shape::Shot),
                false => route(Method::GET, format!("/v1/feedback/{id}"), None, Shape::All),
            })
        },
    },
    Tool {
        name: "update_feedback",
        title: "Triage a feedback item",
        description: "Set a feedback item's status, priority (0 to 3, or null), assignee (who works on it) or resolution (what was done: a pull request's link). To take an item, set status doing and assignee with if_status new (or triaged, planned): it is refused when someone took it first. Set status done with a resolution when the change is made. Needs a write key.",
        schema: || json!({"type": "object", "properties": {
            "id": {"type": "string"},
            "status": {"type": "string", "enum": ["new", "triaged", "planned", "doing", "done", "wontfix"]},
            "priority": {"type": ["integer", "null"], "minimum": 0, "maximum": 3},
            "assignee": {"type": ["string", "null"]},
            "resolution": {"type": ["string", "null"]},
            "kind": {"type": "string", "enum": ["bug", "change", "idea", "question"]},
            "if_status": {"type": "string", "enum": ["new", "triaged", "planned", "doing", "done", "wontfix"]}
        }, "required": ["id"]}),
        read_only: false,
        destructive: false,
        act: |a| {
            let id = encode(s(a, "id")?);
            let mut body = serde_json::Map::new();
            for k in ["status", "priority", "assignee", "resolution", "kind", "if_status"] {
                if let Some(v) = a.get(k) {
                    body.insert(k.to_string(), v.clone());
                }
            }
            Ok(route(Method::PATCH, format!("/v1/feedback/{id}"), Some(Json::Object(body)), Shape::All))
        },
    },
    Tool {
        name: "whoami",
        title: "Who am I",
        description: "The key this connection uses: its name and scope (read, write or admin), which decide what the other tools may change.",
        schema: || json!({"type": "object", "properties": {}}),
        read_only: true,
        destructive: false,
        act: |_| Ok(route(Method::GET, "/v1/whoami", None, Shape::All)),
    },
];

/// `tools/list`'s answer: every tool.
pub fn tools_json() -> Json {
    tools_listed(TOOLS.iter().chain(ops_tools::OPS_TOOLS))
}

/// `tools/list`'s answer for `who`: the operations tools only for a key
/// that may see the server's views ([`crate::ops::may`]).
pub fn tools_for(who: Option<&crate::keys::Principal>) -> Json {
    let ops = who.is_none_or(|w| crate::ops::may(w)["server_views"] == true);
    match ops {
        true => tools_json(),
        false => tools_listed(TOOLS.iter()),
    }
}

fn tools_listed<'a>(tools: impl Iterator<Item = &'a Tool>) -> Json {
    let tools: Vec<Json> = tools
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": (t.schema)(),
                "annotations": {"title": t.title, "readOnlyHint": t.read_only, "destructiveHint": t.destructive, "idempotentHint": t.read_only, "openWorldHint": false},
            })
        })
        .collect();
    json!({"tools": tools})
}

/// The prefix of a saved search's tool: `fn_<name>` calls the stored
/// function `name` (one that `RETURNS TABLE`).
pub const SAVED_PREFIX: &str = "fn_";

/// A parameter type's JSON schema.
fn param_schema(ty: &str) -> Json {
    let t = ty.to_ascii_uppercase();
    match t.split('(').next().unwrap_or("") {
        "INT" => json!({"type": "integer"}),
        "FLOAT" => json!({"type": "number"}),
        "BOOL" => json!({"type": "boolean"}),
        "VECTOR" => json!({"type": "array", "items": {"type": "number"}}),
        "JSON" => json!({}),
        _ if t.ends_with("[]") => json!({"type": "array"}),
        _ => json!({"type": "string"}),
    }
}

/// The saved searches as tools: each stored function that returns a
/// table, by its current version ([`SAVED_PREFIX`]).
pub fn saved_tools(set: &chloedb::query::function::FunctionSet) -> Vec<Json> {
    use chloedb::query::function::Kind;
    let mut out = Vec::new();
    for f in set.currents().filter(|f| f.def.kind == Kind::Table) {
        let name = format!("{SAVED_PREFIX}{}", f.def.name);
        if name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            continue;
        }
        let mut props = serde_json::Map::new();
        let mut required = Vec::new();
        let text = first_text(&f.def);
        for p in &f.def.params {
            let vector = is_vector(&p.ty);
            let mut sch = if vector { json!({"type": ["string", "array"], "items": {"type": "number"}}) } else { param_schema(&p.ty) };
            let what = match (&p.default, vector, text) {
                (Some(d), ..) => format!("{} (default {d})", p.ty),
                (None, true, Some(t)) => format!("{}: words to embed, or a vector; left out, the words of {t:?}", p.ty),
                (None, true, None) => format!("{}: words to embed, or a vector", p.ty),
                (None, false, _) => p.ty.clone(),
            };
            sch["description"] = json!(what);
            props.insert(p.name.clone(), sch);
            if p.default.is_none() && !(vector && text.is_some()) {
                required.push(p.name.clone());
            }
        }
        let about = f.def.description.clone().unwrap_or_else(|| format!("A saved search of this database: {}.", f.def.signature()));
        let title = format!("Saved search {}", f.def.name);
        out.push(json!({
            "name": name,
            "title": title,
            "description": format!("{about} Runs SELECT * FROM {} (version {}) and answers its rows.", f.def.signature(), f.version),
            "inputSchema": {"type": "object", "properties": props, "required": required},
            "annotations": {"title": title, "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
        }));
    }
    out
}

fn is_vector(ty: &str) -> bool {
    ty.to_ascii_uppercase().starts_with("VECTOR")
}

/// The first parameter of type TEXT: the words of a search.
fn first_text(def: &chloedb::query::function::Definition) -> Option<&str> {
    def.params.iter().find(|p| p.ty.eq_ignore_ascii_case("TEXT")).map(|p| p.name.as_str())
}

/// The query a saved search's tool runs: the function called with the
/// arguments given, by name, as parameters.
fn saved_call(set: &chloedb::query::function::FunctionSet, tool: &str, args: &Json) -> Option<Result<Act, Failed>> {
    let name = tool.strip_prefix(SAVED_PREFIX)?;
    let f = set.current(name).filter(|f| f.def.kind == chloedb::query::function::Kind::Table)?;
    let given = args.as_object().cloned().unwrap_or_default();
    if let Some(k) = given.keys().find(|k| !f.def.params.iter().any(|p| &p.name == *k)) {
        return Some(Err(Failed::Arguments(format!("{tool} takes no argument {k:?}: its parameters are {}", f.def.signature()))));
    }
    // A vector parameter takes words, which the server embeds, and is the
    // words of the first text parameter when left out.
    let mut given = given;
    let words = first_text(&f.def).and_then(|t| given.get(t)).and_then(Json::as_str).map(String::from);
    for p in f.def.params.iter().filter(|p| is_vector(&p.ty)) {
        match given.get(&p.name) {
            Some(Json::String(w)) => {
                let w = w.clone();
                given.insert(p.name.clone(), json!({"embed": w}));
            }
            None | Some(Json::Null) if p.default.is_none() => {
                if let Some(w) = &words {
                    given.insert(p.name.clone(), json!({"embed": w}));
                }
            }
            _ => {}
        }
    }
    if let Some(p) = f.def.params.iter().find(|p| p.default.is_none() && given.get(&p.name).is_none_or(Json::is_null)) {
        return Some(Err(Failed::Arguments(format!("{tool} needs {:?} ({})", p.name, p.ty))));
    }
    let named: Vec<String> = f.def.params.iter().filter(|p| given.get(&p.name).is_some_and(|v| !v.is_null())).map(|p| format!("{0} => :{0}", p.name)).collect();
    let sql = format!("SELECT * FROM {}({})", f.def.name, named.join(", "));
    let params: serde_json::Map<String, Json> = given.into_iter().filter(|(_, v)| !v.is_null()).collect();
    Some(Ok(route(Method::POST, "/v1/query", Some(json!({"sql": sql, "params": params})), Shape::Rows(50))))
}

struct Resource {
    uri: &'static str,
    name: &'static str,
    description: &'static str,
    text: &'static str,
}

const RESOURCES: &[Resource] = &[
    Resource { uri: "strata://docs/query-language", name: "ChloeDB query language", description: "The whole SQL reference: types, table options, search functions, ranking, joins, writes.", text: QUERY_LANGUAGE },
    Resource { uri: "strata://docs/build-an-app", name: "Build an app on chloedb-server", description: "A shop in one page: tables, writes, search, stock that never oversells, holds that expire, events counted once.", text: BUILD_AN_APP },
];

fn resources_json() -> Json {
    let list: Vec<Json> = RESOURCES.iter().map(|r| json!({"uri": r.uri, "name": r.name, "description": r.description, "mimeType": "text/markdown", "size": r.text.len()})).collect();
    json!({"resources": list})
}

// ----- the docs, by section

/// A section of a document: its heading path and text.
struct Section {
    doc: &'static str,
    heading: String,
    text: &'static str,
}

fn sections() -> &'static [Section] {
    static S: OnceLock<Vec<Section>> = OnceLock::new();
    S.get_or_init(|| {
        let mut out = Vec::new();
        for (doc, text) in [("query language", QUERY_LANGUAGE), ("build an app", BUILD_AN_APP)] {
            let mut fence = false;
            let mut start = 0;
            let mut h2 = String::new();
            let mut heading = String::new();
            let mut at = 0;
            for line in text.split_inclusive('\n') {
                let t = line.trim_end();
                if t.starts_with("```") {
                    fence = !fence;
                }
                let level = if fence { 0 } else { t.bytes().take_while(|b| *b == b'#').count() };
                if (level == 2 || level == 3) && t.as_bytes().get(level) == Some(&b' ') {
                    if at > start {
                        out.push(Section { doc, heading: heading.clone(), text: &text[start..at] });
                    }
                    let title = t[level + 1..].trim().to_string();
                    if level == 2 {
                        h2 = title.clone();
                        heading = title;
                    } else {
                        heading = format!("{h2} > {title}");
                    }
                    start = at;
                }
                at += line.len();
            }
            if text.len() > start {
                out.push(Section { doc, heading: heading.clone(), text: &text[start..] });
            }
        }
        out
    })
}

fn words(t: &str) -> Vec<String> {
    t.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| w.len() > 1).map(|w| stem(&w.to_lowercase())).collect()
}

/// A word without its plural's ending: `tables` is `table`, `queries`
/// `query`, `boxes` `box`, so a question finds the heading of either.
fn stem(w: &str) -> String {
    if w.len() > 4 && w.ends_with("ies") {
        return format!("{}y", &w[..w.len() - 3]);
    }
    if w.len() > 4 && ["ses", "xes", "ches", "shes"].iter().any(|e| w.ends_with(e)) {
        return w[..w.len() - 2].to_string();
    }
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

/// Words of a question that say nothing of what it is about.
const STOPWORDS: &[&str] = &["the", "an", "of", "to", "in", "into", "for", "on", "and", "or", "with", "is", "are", "how", "do", "doe", "can", "my", "from", "by", "it", "this", "that", "what", "which", "example", "way", "use", "want", "need"];

/// A question's words, without stopwords, and with the names ChloeDB
/// gives what other databases call otherwise (a materialized view is a
/// rollup).
fn query_words(query: &str) -> Vec<String> {
    let mut q: Vec<String> = words(query).into_iter().filter(|w| !STOPWORDS.contains(&w.as_str())).collect();
    for (other, ours) in [("materialized", "rollup"), ("materialised", "rollup"), ("precompute", "rollup"), ("precomputed", "rollup"), ("today", "current_date")] {
        if q.iter().any(|w| w == other) {
            q.push(ours.to_string());
        }
    }
    q
}

/// The sections that best match `query`: each query word scores by how
/// rare it is among the sections (as BM25's idf), its count in the
/// section (at most 5), and three times over in the heading.
pub fn search_docs(query: &str, limit: usize) -> Json {
    let all = sections();
    let mut q = query_words(query);
    // Words side by side in the question (`create table`), which a
    // section that has them side by side matches more than one with each.
    let pairs: Vec<(String, String)> = q.windows(2).map(|w| (w[0].clone(), w[1].clone())).collect();
    q.sort();
    q.dedup();
    let texts: Vec<(Vec<String>, Vec<String>)> = all.iter().map(|s| (words(&s.heading), words(s.text))).collect();
    let n = all.len() as f64;
    let idf: Vec<f64> = q.iter().map(|w| {
        let df = texts.iter().filter(|(_, t)| t.iter().any(|x| x == w)).count() as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }).collect();
    let mut scored: Vec<(f64, usize)> = texts
        .iter()
        .enumerate()
        .map(|(i, (h, t))| {
            let score: f64 = q.iter().zip(&idf).map(|(w, idf)| {
                let tf = t.iter().filter(|x| *x == w).count().min(5) as f64;
                let hf = h.iter().filter(|x| *x == w).count() as f64;
                idf * (tf + 3.0 * hf)
            }).sum();
            let side_by_side: f64 = pairs.iter().map(|(a, b)| {
                let k = t.windows(2).filter(|w| w[0] == *a && w[1] == *b).count().min(5) as f64;
                let both = q.iter().zip(&idf).filter(|(w, _)| *w == a || *w == b).map(|(_, f)| f).sum::<f64>();
                k * both
            }).sum();
            (score + side_by_side, i)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let hits: Vec<Json> = scored
        .iter()
        .take(limit)
        .map(|&(score, i)| {
            let s = &all[i];
            let (text, cut) = clip(s.text, 6000);
            json!({"doc": s.doc, "section": s.heading, "score": (score * 100.0).round() / 100.0, "text": text, "cut": cut})
        })
        .collect();
    json!({"sections": hits, "searched": all.len()})
}

/// `t` cut to at most `max` bytes at a character boundary, and whether
/// it was cut.
fn clip(t: &str, max: usize) -> (&str, bool) {
    if t.len() <= max {
        return (t, false);
    }
    let mut end = max;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    (&t[..end], true)
}

// ----- import

/// The `import` tool's call: the route with its query string, and the rows.
fn import_act(a: &Json) -> Result<Act, Failed> {
    let table = s(a, "table")?;
    let (format, data) = match (a.get("rows"), opt_s(a, "data")) {
        (Some(Json::Array(rows)), None) if !rows.is_empty() => ("json", serde_json::to_vec(rows).map_err(|e| Failed::Arguments(e.to_string()))?),
        (None | Some(Json::Null), Some(d)) => (opt_s(a, "format").unwrap_or("csv"), d.as_bytes().to_vec()),
        (Some(_), Some(_)) => return Err(Failed::Arguments("send the rows in data (text) or in rows (objects), not both".into())),
        _ => return Err(Failed::Arguments("the arguments need the rows: data, as text (a CSV file's contents), or rows, an array of objects".into())),
    };
    let create = match a.get("create").and_then(|v| v.as_bool()) {
        Some(true) => {
            let f = chloedb::ingest::Format::parse(format).ok_or_else(|| Failed::Arguments(format!("format is csv, tsv, jsonl or json, not {format:?}")))?;
            Some(chloedb::ingest::create_for(table, &data, f, opt_s(a, "key")).map_err(|e| Failed::Arguments(format!("no table could be made from the rows: {e}")))?)
        }
        _ => None,
    };
    let content_type = match format.to_ascii_lowercase().as_str() {
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "json" => "application/json",
        f => return Err(Failed::Arguments(format!("format is csv, tsv, jsonl or json, not {f:?}"))),
    };
    let mut path = format!("/v1/import?table={}", encode(table));
    if let Some(e) = opt_s(a, "empty") {
        path.push_str(&format!("&empty={}", encode(e)));
    }
    if a.get("strict").and_then(|v| v.as_bool()) == Some(true) {
        path.push_str("&strict=true");
    }
    Ok(Act::Import { path, content_type, data, create })
}

// ----- check_sql

/// What a statement would do, bound against the tables and not run.
pub fn check_sql(sql: &str, tables: &[std::sync::Arc<chloedb::Schema>]) -> Result<Json, String> {
    use chloedb::query::sql::{bind_command, Command, Write};
    if let Some(j) = check_own_statement(sql, tables)? {
        return Ok(j);
    }
    let refs: Vec<&chloedb::Schema> = tables.iter().map(|t| &**t).collect();
    let cmd = bind_command(sql, &refs, &chloedb::query::Params::default()).map_err(|e| e.to_string())?;
    Ok(match cmd {
        Command::Query => json!({"statement": "select", "note": "a SELECT is bound when it runs: explain shows its plan, or an error"}),
        Command::Write { table, write } => {
            let kind = match write {
                Write::Insert { docs, .. } => json!({"statement": "insert", "rows": docs.len()}),
                Write::Update { sets, .. } => json!({"statement": "update", "fields_set": sets.len()}),
                Write::Delete { .. } => json!({"statement": "delete"}),
            };
            let mut k = kind;
            k["table"] = json!(table);
            k["needs"] = json!("a write key");
            k
        }
        Command::CreateTable { schema, if_not_exists } => json!({
            "statement": "create table", "table": schema.table, "if_not_exists": if_not_exists,
            "exists": refs.iter().any(|t| t.table.eq_ignore_ascii_case(&schema.table)),
            "sql": chloedb::query::ddl::create_sql(&schema), "short": schema.to_short_json(), "needs": "an admin key",
        }),
        Command::DropTable { tables, .. } => json!({"statement": "drop table", "tables": tables, "needs": "an admin key"}),
        Command::AlterTable { table, alters } => json!({"statement": "alter table", "table": table, "changes": alters.len(), "needs": "an admin key"}),
        Command::CreateRankProfile { table, replace, .. } => json!({"statement": "create rank profile", "table": table, "replace": replace, "needs": "an admin key"}),
        Command::DropRankProfile { table, name, .. } => json!({"statement": "drop rank profile", "table": table, "name": name, "needs": "an admin key"}),
        Command::Function(f) => json!({"statement": if matches!(f, chloedb::query::function::Statement::Drop { .. }) { "drop function" } else { "create function" }, "name": f.name(), "needs": "an admin key"}),
        Command::Explain { json } => json!({"statement": "explain", "json": json, "note": "run it with query: it answers the plan without running the statement"}),
        Command::ShowTables => json!({"statement": "show tables", "note": "the names of the tables, a row each"}),
    })
}

/// The statements `execute` takes before SQL's own (as `POST /v1/sql`
/// does, [`crate::service::Service::sql_in`]): rollups, classifiers,
/// recommenders and a table's synonyms, checked against the tables.
fn check_own_statement(sql: &str, tables: &[std::sync::Arc<chloedb::Schema>]) -> Result<Option<Json>, String> {
    use chloedb::query::{classify, recommend, rollup, table_synonyms};
    let find = |t: &str| tables.iter().find(|s| s.table.eq_ignore_ascii_case(t)).ok_or_else(|| format!("query error: no table {t}"));
    if let Some(st) = rollup::parse_statement(sql).map_err(|e| e.to_string())? {
        return Ok(Some(match st {
            rollup::Statement::Create { rollup: r, if_not_exists } => {
                let base = find(&r.base)?;
                let columns = r.check(base).map_err(|e| e.to_string())?;
                json!({
                    "statement": "create rollup", "rollup": r.name, "table": base.table, "if_not_exists": if_not_exists,
                    "exists": tables.iter().any(|t| t.table.eq_ignore_ascii_case(&r.name)),
                    "columns": columns.iter().map(|(c, ty)| json!({"name": c, "type": ty})).collect::<Vec<_>>(),
                    "needs": "an admin key not limited to tables",
                    "note": "the rollup is a table of its own, filled a few seconds after it is made and kept up to date as rows come",
                })
            }
            rollup::Statement::Drop { name, if_exists } => json!({"statement": "drop rollup", "rollup": name, "if_exists": if_exists, "needs": "an admin key not limited to tables"}),
            rollup::Statement::Refresh { name, full } => json!({"statement": "refresh rollup", "rollup": name, "full": full, "needs": "an admin key not limited to tables"}),
        }));
    }
    if let Some(st) = classify::parse_statement(sql).map_err(|e| e.to_string())? {
        return Ok(Some(match st {
            classify::Statement::Create(c) => {
                let schema = find(&c.table)?;
                let model = c.check(schema).map_err(|e| e.to_string())?;
                json!({"statement": "create classifier", "name": c.name, "table": schema.table, "field": c.field, "labels": c.labels, "embedded_by": model, "needs": "an admin key"})
            }
            classify::Statement::Drop { name, if_exists } => json!({"statement": "drop classifier", "name": name, "if_exists": if_exists, "needs": "an admin key"}),
        }));
    }
    if let Some(st) = recommend::parse_statement(sql).map_err(|e| e.to_string())? {
        return Ok(Some(match st {
            recommend::Statement::Create { def, replace } => {
                def.check(tables).map_err(|e| e.to_string())?;
                json!({"statement": "create recommender", "name": def.name, "events": def.events, "replace": replace, "needs": "an admin key"})
            }
            recommend::Statement::Drop { name, if_exists } => json!({"statement": "drop recommender", "name": name, "if_exists": if_exists, "needs": "an admin key"}),
        }));
    }
    if let Some(st) = table_synonyms::parse_statement(sql).map_err(|e| e.to_string())? {
        let schema = find(st.table())?;
        st.function_sql().map_err(|e| e.to_string())?;
        let what = if matches!(st, table_synonyms::Statement::Create { .. }) { "create synonyms" } else { "drop synonyms" };
        return Ok(Some(json!({"statement": what, "table": schema.table, "needs": "an admin key"})));
    }
    Ok(None)
}

// ----- the protocol

/// What a message is answered with: the request's credential and where
/// it came from, to send each tool's route with.
pub struct Caller<'a> {
    pub headers: &'a HeaderMap,
    pub peer: Option<SocketAddr>,
    /// Who sent the message, as the route's check found ([`crate::keys`]):
    /// `tools/list` offers the operations tools to who may use them.
    pub who: Option<&'a crate::keys::Principal>,
}

fn error(id: &Json, code: i64, message: impl Into<String>) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn ok(id: &Json, result: Json) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// `POST /v1/mcp`: one message or a batch. A request is answered with
/// JSON; notifications and responses alone with 202 and no body.
pub async fn serve(svc: &std::sync::Arc<Service>, router: Option<Router>, caller: Caller<'_>, body: &[u8]) -> Response {
    let msg: Json = match serde_json::from_slice(body) {
        Ok(m) => m,
        Err(e) => return (StatusCode::BAD_REQUEST, axum::Json(error(&Json::Null, -32700, format!("parse error: {e}")))).into_response(),
    };
    let answer = match msg {
        Json::Array(batch) if batch.is_empty() => Some(error(&Json::Null, -32600, "an empty batch")),
        Json::Array(batch) => {
            let mut out = Vec::new();
            for m in &batch {
                if let Some(a) = handle(svc, router.as_ref(), &caller, m).await {
                    out.push(a);
                }
            }
            (!out.is_empty()).then_some(Json::Array(out))
        }
        m => handle(svc, router.as_ref(), &caller, &m).await,
    };
    match answer {
        Some(a) => axum::Json(a).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// One message's answer; none for a notification or a response.
pub async fn handle(svc: &std::sync::Arc<Service>, router: Option<&Router>, caller: &Caller<'_>, m: &Json) -> Option<Json> {
    if !m.is_object() {
        return Some(error(&Json::Null, -32600, "a message is a JSON object"));
    }
    let id = m.get("id").cloned();
    let method = match m.get("method").and_then(|v| v.as_str()) {
        Some(x) => x,
        // A response to a request of ours (this server sends none), or junk.
        None => return id.filter(|_| m.get("result").is_none() && m.get("error").is_none()).map(|id| error(&id, -32600, "a request names its method")),
    };
    let id = id?; // A notification: nothing to answer.
    if m.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Some(error(&id, -32600, "jsonrpc must be \"2.0\""));
    }
    let params = m.get("params").cloned().unwrap_or(Json::Null);
    Some(match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(VERSIONS[0]);
            let version = VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(VERSIONS[0]);
            ok(&id, json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}, "resources": {"listChanged": false, "subscribe": false}},
                "serverInfo": {"name": "chloedb", "title": "ChloeDB", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" | "logging/setLevel" => ok(&id, json!({})),
        "tools/list" => {
            svc.learn_functions().await;
            let mut j = tools_for(caller.who);
            if let Some(list) = j["tools"].as_array_mut() {
                list.extend(saved_tools(&svc.functions()));
            }
            ok(&id, j)
        }
        "resources/list" => ok(&id, resources_json()),
        "resources/templates/list" => ok(&id, json!({"resourceTemplates": []})),
        "prompts/list" => ok(&id, json!({"prompts": []})),
        "resources/read" => {
            let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
            match RESOURCES.iter().find(|r| r.uri == uri) {
                Some(r) => ok(&id, json!({"contents": [{"uri": r.uri, "mimeType": "text/markdown", "text": r.text}]})),
                None => error(&id, -32002, format!("no resource {uri:?}: resources/list names them")),
            }
        }
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call(svc, router, caller, name, &args).await {
                Ok(content) => ok(&id, json!({"content": content, "isError": false})),
                Err(Failed::Call(why)) => ok(&id, json!({"content": [{"type": "text", "text": why}], "isError": true})),
                Err(Failed::Arguments(why)) => error(&id, -32602, why),
            }
        }
        other => error(&id, -32601, format!("no method {other:?}")),
    })
}

/// A tool's call: its answer's content, text and perhaps an image.
async fn call(svc: &std::sync::Arc<Service>, router: Option<&Router>, caller: &Caller<'_>, name: &str, args: &Json) -> Result<Vec<Json>, Failed> {
    if !args.is_object() {
        return Err(Failed::Arguments("arguments are an object".into()));
    }
    let act = match TOOLS.iter().chain(ops_tools::OPS_TOOLS).find(|t| t.name == name) {
        Some(tool) => (tool.act)(args)?,
        None => {
            if name.starts_with(SAVED_PREFIX) {
                svc.learn_functions().await;
            }
            saved_call(&svc.functions(), name, args).ok_or_else(|| Failed::Arguments(format!("no tool {name:?}: tools/list names them")))??
        }
    };
    let mut image = None;
    let answer = match act {
        Act::CheckSql(sql) => check_sql(&sql, &svc.tables_listed().await).map_err(Failed::Call)?,
        Act::SearchDocs { query, limit } => search_docs(&query, limit),
        Act::Runbook { query } => crate::runbooks::find(&query),
        Act::Action { method, path, body, reason } => {
            let router = router.ok_or_else(|| Failed::Call("the server is still starting".into()))?;
            forward_with(router, caller, method, &path, body, &[("x-strata-reason", reason)]).await.map_err(Failed::Call)?
        }
        Act::Import { path, content_type, data, create } => {
            let router = router.ok_or_else(|| Failed::Call("the server is still starting".into()))?;
            let made = match &create {
                Some(sql) => Some(forward(router, caller, Method::POST, "/v1/sql", Some(json!({"sql": sql}))).await.map_err(|e| Failed::Call(format!("{e} (the import would make the table with {sql})")))?),
                None => None,
            };
            let mut j = forward_body(router, caller, Method::POST, &path, Some((content_type, data)), &[]).await.map_err(Failed::Call)?;
            if let (Some(sql), Some(_)) = (create, made) {
                // IF NOT EXISTS: a table that was there already is kept as it was.
                j["create_table"] = json!(sql);
            }
            j
        }
        Act::Route { method, path, body, shape } => {
            let router = router.ok_or_else(|| Failed::Call("the server is still starting".into()))?;
            let mut j = forward(router, caller, method, &path, body).await.map_err(Failed::Call)?;
            if let Shape::Shot = shape {
                image = j.as_object_mut().and_then(|o| o.remove("shot")).and_then(|s| s.as_str().and_then(data_url_image));
            }
            shaped(j, shape)
        }
    };
    let text = serde_json::to_string(&answer).unwrap_or_default();
    let text = match clip(&text, MAX_TEXT) {
        (t, false) => t.to_string(),
        (t, true) => format!("{t}\n[cut at {MAX_TEXT} characters of {}: ask for fewer rows or columns]", text.len()),
    };
    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend(image);
    Ok(content)
}

/// A header's value from any text: percent-encoded past ASCII, as
/// `x-chloedb-reason` is read ([`crate::http`]'s `header_text`).
pub(crate) fn encode_header(v: &str) -> String {
    v.bytes().map(|b| if (0x20..0x7f).contains(&b) && b != b'%' { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// An MCP image content block of a `data:` URL's picture.
fn data_url_image(u: &str) -> Option<Json> {
    let (mime, data) = u.strip_prefix("data:")?.split_once(";base64,")?;
    Some(json!({"type": "image", "mimeType": mime, "data": data}))
}

/// What a tool keeps of its route's answer.
fn shaped(mut j: Json, shape: Shape) -> Json {
    match shape {
        Shape::All | Shape::Shot => j,
        Shape::Tables => {
            if let Some(ts) = j.get_mut("tables").and_then(|t| t.as_array_mut()) {
                for t in ts.iter_mut() {
                    if let Some(o) = t.as_object_mut() {
                        o.remove("schema");
                        o.remove("short");
                        if let Some(Json::Array(p)) = o.get_mut("profiles") {
                            *p = p.iter().map(|x| x.get("name").cloned().unwrap_or_else(|| x.clone())).collect();
                        }
                    }
                }
            }
            j
        }
        Shape::Rows(max) => {
            if let Some(rows) = j.get_mut("rows").and_then(|r| r.as_array_mut()) {
                let n = rows.len();
                if n > max {
                    rows.truncate(max);
                    j["truncated"] = json!({"rows": n, "shown": max});
                }
            }
            j
        }
    }
}

/// A request to this server's own routes, with the caller's credential:
/// its JSON answer, or why it failed (`<status>: <error>`).
pub(crate) async fn forward(router: &Router, caller: &Caller<'_>, method: Method, path: &str, body: Option<Json>) -> Result<Json, String> {
    forward_with(router, caller, method, path, body, &[]).await
}

/// [`forward`], with headers of its own (`extra`, which win over the
/// caller's).
pub(crate) async fn forward_with(router: &Router, caller: &Caller<'_>, method: Method, path: &str, body: Option<Json>, extra: &[(&str, String)]) -> Result<Json, String> {
    let body = body.map(|j| serde_json::to_vec(&j).map(|b| ("application/json", b))).transpose().map_err(|e| e.to_string())?;
    forward_body(router, caller, method, path, body, extra).await
}

/// [`forward_with`], with a body of any content type.
async fn forward_body(router: &Router, caller: &Caller<'_>, method: Method, path: &str, body: Option<(&str, Vec<u8>)>, extra: &[(&str, String)]) -> Result<Json, String> {
    use tower::ServiceExt;
    let mut b = Request::builder().method(method).uri(path);
    let passed = [
        header::AUTHORIZATION,
        header::HOST,
        header::USER_AGENT,
        header::HeaderName::from_static(crate::hub::VIA_PROXY_AUTH),
        header::HeaderName::from_static("x-forwarded-for"),
        header::HeaderName::from_static("x-strata-reason"),
        // ops: what the agent says of itself, for the audit log.
        header::HeaderName::from_static("x-strata-agent-model"),
        header::HeaderName::from_static("x-strata-agent-session"),
        header::HeaderName::from_static("x-strata-agent-run"),
        // The change request an approval applies ([`crate::changes`]).
        header::HeaderName::from_static("x-strata-change"),
    ];
    for h in passed {
        if extra.iter().any(|(n, _)| *n == h.as_str()) {
            continue;
        }
        if let Some(v) = caller.headers.get(&h) {
            b = b.header(h, v.clone());
        }
    }
    for (n, v) in extra {
        // A header's value is one line of visible text.
        let v: String = v.chars().map(|c| if c.is_control() { ' ' } else { c }).take(500).collect();
        let v = crate::mcp::encode_header(&v);
        b = b.header(*n, v);
    }
    // Who the query log and Access name: the MCP client, as `mcp`.
    let app = caller.headers.get("x-strata-app").cloned().unwrap_or(HeaderValue::from_static("mcp"));
    b = b.header("x-strata-app", app);
    if let Some(id) = caller.headers.get(crate::obs::HEADER) {
        b = b.header(crate::obs::HEADER, id.clone());
    }
    let body = match body {
        Some((content_type, bytes)) => {
            b = b.header(header::CONTENT_TYPE, content_type);
            Body::from(bytes)
        }
        None => Body::empty(),
    };
    let mut req = b.body(body).map_err(|e| e.to_string())?;
    if let Some(p) = caller.peer {
        req.extensions_mut().insert(axum::extract::ConnectInfo(p));
    }
    let res = match router.clone().oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), MAX_BODY).await.map_err(|e| format!("{status}: reading the answer: {e}"))?;
    let j: Json = serde_json::from_slice(&bytes).unwrap_or_else(|_| Json::String(String::from_utf8_lossy(&bytes).into_owned()));
    if !status.is_success() {
        let why = match j.get("error").and_then(|e| e.as_str()) {
            Some(e) => e.to_string(),
            None if bytes.is_empty() => status.canonical_reason().unwrap_or("no answer").to_string(),
            None => j.to_string(),
        };
        // errors: the code and what to do, for the model to act on.
        let code = match j.get("error_code").and_then(|c| c.as_str()) {
            Some(c) => format!(" [code {c}{}]", j.get("hint").and_then(|h| h.as_str()).map(|h| format!(": {h}")).unwrap_or_default()),
            None => String::new(),
        };
        return Err(format!("{}: {why}{code}", status.as_u16()));
    }
    Ok(j)
}

// ----- stdio

/// `chloedb-server mcp --url URL`: MCP over stdin and stdout, each message
/// sent to `URL/v1/mcp` with the key, each answer written back as one
/// line. A server that cannot be reached answers the request with an error.
pub async fn stdio(url: &str, key: Option<&str>, ca: Option<Vec<u8>>) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut builder = crate::http_client();
    if let Some(pem) = ca {
        let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| format!("--ca: {e}"))?;
        builder = builder.add_root_certificate(cert);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let endpoint = format!("{}/v1/mcp", url.trim_end_matches('/'));
    // ops: this bridge is one agent's session, which the audit log names;
    // the model, when the client's environment says it.
    let session = format!("stdio-{}-{:x}", std::process::id(), crate::telemetry::now_ms());
    let model = std::env::var("CHLOEDB_AGENT_MODEL").ok().filter(|m| !m.trim().is_empty()).map(|m| encode_header(m.trim()));
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        if line.trim().is_empty() {
            continue;
        }
        let id = serde_json::from_str::<Json>(&line).ok().and_then(|m| m.get("id").cloned());
        let mut r = client.post(&endpoint).header(header::CONTENT_TYPE, "application/json").header(header::ACCEPT, "application/json, text/event-stream").header("x-strata-app", "mcp-stdio").header("x-chloedb-agent-session", &session);
        if let Some(m) = &model {
            r = r.header("x-chloedb-agent-model", m);
        }
        if let Some(k) = key {
            r = r.bearer_auth(k);
        }
        let answer = match r.body(line).send().await {
            Ok(res) if res.status() == reqwest::StatusCode::ACCEPTED => None,
            Ok(res) => {
                let status = res.status();
                match res.text().await {
                    Ok(t) if serde_json::from_str::<Json>(&t).is_ok_and(|j| j.get("jsonrpc").is_some() || j.is_array()) => Some(t),
                    Ok(t) => id.map(|id| error(&id, -32000, format!("{endpoint} answered {status}: {}", clip(&t, 500).0)).to_string()),
                    Err(e) => id.map(|id| error(&id, -32000, format!("{endpoint}: {e}")).to_string()),
                }
            }
            Err(e) => id.map(|id| error(&id, -32000, format!("{endpoint} cannot be reached: {e}")).to_string()),
        };
        if let Some(a) = answer {
            // One message a line: JSON with no newline inside.
            let one = serde_json::from_str::<Json>(&a).map(|j| j.to_string()).unwrap_or(a);
            out.write_all(one.as_bytes()).await.map_err(|e| e.to_string())?;
            out.write_all(b"\n").await.map_err(|e| e.to_string())?;
            out.flush().await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_search_finds_what_other_databases_call_otherwise() {
        let first = |q: &str| search_docs(q, 3)["sections"][0]["section"].as_str().unwrap_or("").to_string();
        for (q, want) in [
            ("materialized view", "Rollups"),
            ("precompute daily counts kept up to date", "Rollups"),
            ("CREATE TABLE example", "Tables"),
            ("import a CSV file into a table", "Loading a file"),
            ("current date today", "Parts of times"),
        ] {
            let got = first(q);
            assert!(got.contains(want), "{q:?} found {got:?}, not {want}");
        }
        assert_eq!(stem("queries"), "query");
        assert_eq!(stem("tables"), "table");
        assert_eq!(stem("class"), "class");
    }

    #[test]
    fn docs_split_into_sections_and_search_finds_them() {
        assert!(sections().len() > 20, "{}", sections().len());
        assert!(sections().iter().all(|s| !s.heading.is_empty() || s.doc == "query language" || s.doc == "build an app"));
        let j = search_docs("bm25 fuzzy typos", 3);
        let hits = j["sections"].as_array().unwrap();
        assert!(!hits.is_empty(), "{j}");
        assert!(hits.iter().any(|h| h["text"].as_str().unwrap().to_lowercase().contains("fuzzy")), "{j}");
        let j = search_docs("expire_by holds", 2);
        assert!(j["sections"].as_array().unwrap().iter().any(|h| h["text"].as_str().unwrap().contains("expire_by")), "{j}");
        assert_eq!(search_docs("zzqqxx", 3)["sections"], json!([]));
    }

    #[test]
    fn saved_searches_are_tools_with_their_parameters() {
        use chloedb::query::function::{Definition, Function, FunctionSet, Kind, Param};
        let def = |name: &str, kind| Definition {
            name: name.into(),
            kind,
            params: vec![Param { name: "q".into(), ty: "TEXT".into(), default: None }, Param { name: "k".into(), ty: "INT".into(), default: Some("10".into()) }],
            returns: if kind == Kind::Table { "TABLE".into() } else { "FLOAT".into() },
            body: if kind == Kind::Table { "SELECT 1".into() } else { "1.0".into() },
            description: Some("Finds help articles.".into()),
        };
        let set = FunctionSet::of([Function::new(def("recall", Kind::Table), 2, None, None).unwrap(), Function::new(def("boost", Kind::Scalar), 1, None, None).unwrap()]);
        let tools = saved_tools(&set);
        assert_eq!(tools.len(), 1, "only table functions: {tools:?}");
        let t = &tools[0];
        assert_eq!(t["name"], "fn_recall");
        assert_eq!(t["inputSchema"]["required"], json!(["q"]));
        assert_eq!(t["inputSchema"]["properties"]["k"]["type"], "integer");
        assert!(t["description"].as_str().unwrap().starts_with("Finds help articles."), "{t}");
        match saved_call(&set, "fn_recall", &json!({"q": "tokio"})) {
            Some(Ok(Act::Route { path, body: Some(b), .. })) => {
                assert_eq!(path, "/v1/query");
                assert_eq!(b["sql"], "SELECT * FROM recall(q => :q)");
                assert_eq!(b["params"], json!({"q": "tokio"}));
            }
            _ => panic!("a query"),
        }
        assert!(matches!(saved_call(&set, "fn_recall", &json!({})), Some(Err(Failed::Arguments(_)))));
        assert!(matches!(saved_call(&set, "fn_recall", &json!({"q": "x", "zz": 1})), Some(Err(Failed::Arguments(_)))));
        assert!(saved_call(&set, "fn_boost", &json!({})).is_none());
        // A vector parameter is the words of the first text one when left out.
        let mut both = def("both", Kind::Table);
        both.params.push(Param { name: "v".into(), ty: "VECTOR(4)".into(), default: None });
        let set = FunctionSet::of([Function::new(both, 1, None, None).unwrap()]);
        assert_eq!(saved_tools(&set)[0]["inputSchema"]["required"], json!(["q"]));
        match saved_call(&set, "fn_both", &json!({"q": "tokio"})) {
            Some(Ok(Act::Route { body: Some(b), .. })) => {
                assert_eq!(b["sql"], "SELECT * FROM both(q => :q, v => :v)");
                assert_eq!(b["params"]["v"], json!({"embed": "tokio"}));
            }
            _ => panic!("a query"),
        }
        assert!(saved_call(&set, "fn_nothing", &json!({})).is_none());
    }

    #[test]
    fn every_tool_has_a_schema_of_an_object() {
        let j = tools_json();
        let tools = j["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOLS.len() + ops_tools::OPS_TOOLS.len());
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        }
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "names are unique: {names:?}");
    }

    #[test]
    fn values_are_percent_encoded() {
        assert_eq!(encode("a b/c=d&é"), "a%20b%2Fc%3Dd%26%C3%A9");
        assert_eq!(encode("attrs.dims-w_1~"), "attrs.dims-w_1~");
    }

    #[test]
    fn rows_past_the_limit_are_cut_and_say_so() {
        let j = shaped(json!({"columns": ["a"], "rows": [[1], [2], [3]]}), Shape::Rows(2));
        assert_eq!(j["rows"], json!([[1], [2]]));
        assert_eq!(j["truncated"], json!({"rows": 3, "shown": 2}));
        let j = shaped(json!({"rows": [[1]]}), Shape::Rows(2));
        assert!(j.get("truncated").is_none());
    }
}
