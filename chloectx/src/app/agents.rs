//! What an agent reaches of the app: the grants of a key limited to tables,
//! and a space as a Model Context Protocol server.
//!
//! **Grants.** chloedb-server forwards `/v1/apps/chloectx/...` with the
//! app's token and says who called in headers it sets itself; for a key
//! limited to tables, `x-chloedb-tables` (`ctx_billing=admin,*=read`). The
//! app writes with a key of its own, so it holds such a caller to its
//! grants here: a space is its table `ctx_<space>` (read to read it and ask
//! it, write to add, replace or retire notes and hand over exchanges, admin
//! to create it; building apps from it is an unlimited admin's), the incidents are `ctx_incidents`,
//! the Inspector reads `chloectx_memory`, and the machine's local stores
//! are no limited key's. A caller without the header (an admin, the
//! server's token) is not limited.
//!
//! **MCP.** `POST /v1/spaces/<space>/mcp` answers the protocol's JSON-RPC
//! (streamable HTTP, JSON answers only) with three tools on that space:
//! `ask` (the context for a question, within a budget of tokens),
//! `remember` (a note, which may replace an older one) and `notes` (the
//! current ones). So Claude Code, or any MCP client, connects to a space
//! with `claude mcp add --transport http ctx-<space> <server>/v1/apps/chloectx/v1/spaces/<space>/mcp
//! --header "Authorization: Bearer <key>"`, a key limited to `ctx_<space>`.

use serde_json::{json, Value as Json};

use super::{bad, table_of, App, Fail};
use crate::ui::Request;

/// The scopes of chloedb-server's keys, weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Feed,
    Read,
    Write,
    Admin,
}

impl Scope {
    fn parse(s: &str) -> Option<Scope> {
        Some(match s.trim() {
            "feed" => Scope::Feed,
            "read" => Scope::Read,
            "write" => Scope::Write,
            "admin" => Scope::Admin,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Scope::Feed => "feed",
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Admin => "admin",
        }
    }
}

/// A key's grants on tables, as `x-chloedb-tables` lists them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grants(Vec<(String, Scope)>);

impl Grants {
    /// The grants of `x-chloedb-tables`: `table=scope`, comma-separated, a
    /// table a name, a prefix ending in `*`, or `*`. A grant that does not
    /// read is left out (it gives nothing).
    pub fn parse(header: &str) -> Grants {
        Grants(
            header
                .split(',')
                .filter_map(|g| {
                    let (t, s) = g.split_once('=').unwrap_or((g, "read"));
                    Some((t.trim().to_ascii_lowercase(), Scope::parse(s)?))
                })
                .filter(|(t, _)| !t.is_empty())
                .collect(),
        )
    }

    /// The most they give on `table`.
    pub fn on(&self, table: &str) -> Option<Scope> {
        let table = table.to_ascii_lowercase();
        self.0
            .iter()
            .filter(|(t, _)| match t.strip_suffix('*') {
                Some(prefix) => table.starts_with(prefix),
                None => *t == table,
            })
            .map(|(_, s)| *s)
            .max()
    }

    fn need(&self, table: &str, scope: Scope, what: &str) -> Result<(), Fail> {
        match self.on(table) {
            Some(s) if s >= scope => Ok(()),
            _ => Err(Fail(403, format!("this key may not {what}: it needs {} on table {table}", scope.name()))),
        }
    }
}

/// The grants of the caller of `req`, when its key is limited to tables.
pub fn grants_of(req: &Request) -> Option<Grants> {
    req.header("x-chloedb-tables").map(Grants::parse)
}

/// Refuses what a key limited to tables may not do (see the module's
/// documentation). `p` is the request's path, split.
pub fn check(g: &Grants, method: &str, p: &[&str]) -> Result<(), Fail> {
    let space = |s: &str| table_of(s);
    match (method, p) {
        ("GET", ["v1", "info"]) | ("GET", ["v1", "spaces"]) => Ok(()),
        ("POST", ["v1", "spaces"]) => Err(Fail(403, "a key limited to tables may not create a space: an admin's key may".into())),
        ("GET", ["v1", "spaces", s, ..]) => g.need(&space(s), Scope::Read, &format!("read the space {s}")),
        // Asking writes nothing; MCP's tools are checked one by one.
        ("POST", ["v1", "spaces", s, "ask" | "mcp"]) => g.need(&space(s), Scope::Read, &format!("ask the space {s}")),
        // Apps built from a space spend the app engine's model calls: an
        // admin's, not an agent's.
        ("POST", ["v1", "spaces", _, "apps", ..]) => Err(Fail(403, "a key limited to tables may not build apps: an admin's key may".into())),
        (_, ["v1", "spaces", s, ..]) => g.need(&space(s), Scope::Write, &format!("change the space {s}")),
        ("GET", ["v1", "incidents", ..]) => g.need(&space("incidents"), Scope::Read, "read the incidents"),
        (_, ["v1", "incidents", ..]) => g.need(&space("incidents"), Scope::Write, "record or draft incidents"),
        // Each project's table, as the call reads it (`App::memory_table`).
        ("GET", ["v1", "memory", ..]) => Ok(()),
        (_, ["v1", "local", ..]) => Err(Fail(403, "the local stores are this machine's sessions: no key limited to tables reads them".into())),
        _ => Err(Fail(403, "a key limited to tables may not call this".into())),
    }
}

/// The protocol versions answered, the latest first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

fn tools() -> Json {
    json!([
        {
            "name": "ask",
            "description": "The context this memory space holds for a task or question: its rules, then the notes (decisions, facts, plans) a search finds, newest version of each, within a budget of tokens. Call it before starting work that earlier decisions may constrain.",
            "inputSchema": {"type": "object", "properties": {
                "question": {"type": "string", "description": "the task or question, in words"},
                "budget": {"type": "integer", "description": "the most tokens of context (default 2000)", "minimum": 200, "maximum": 50000}
            }, "required": ["question"]}
        },
        {
            "name": "remember",
            "description": "Write down one thing worth keeping in this memory space: a decision, a rule (a standing preference), a fact, a todo or a note. One short self-contained sentence or two. To change an earlier note, pass its id as replaces: the old one is kept as history.",
            "inputSchema": {"type": "object", "properties": {
                "kind": {"type": "string", "enum": ["decision", "rule", "fact", "todo", "note"]},
                "text": {"type": "string"},
                "replaces": {"type": "string", "description": "the id of the note this one replaces (from ask or notes)"},
                "entities": {"type": "array", "items": {"type": "string"}, "description": "names the note is about"}
            }, "required": ["kind", "text"]}
        },
        {
            "name": "notes",
            "description": "The space's current notes, newest first, with their ids: to review them or find the one to replace.",
            "inputSchema": {"type": "object", "properties": {
                "kind": {"type": "string", "enum": ["decision", "rule", "fact", "todo", "note"]},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200}
            }}
        }
    ])
}

fn note_line(n: &Json) -> String {
    format!("- [{}] {}: {}", n["id"].as_str().unwrap_or(""), n["kind"].as_str().unwrap_or(""), n["text"].as_str().unwrap_or(""))
}

impl App {
    /// One JSON-RPC message of MCP for `space`; `None` for a notification.
    fn mcp_one(&self, space: &str, grants: Option<&Grants>, m: &Json) -> Option<Json> {
        let id = m.get("id").cloned();
        let method = m["method"].as_str().unwrap_or("");
        // A notification (no id) is taken and not answered.
        let id = id?;
        let fail = |code: i64, message: String| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        let ok = |result: Json| json!({"jsonrpc": "2.0", "id": id, "result": result});
        Some(match method {
            "initialize" => {
                let asked = m["params"]["protocolVersion"].as_str().unwrap_or("");
                let version = VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(VERSIONS[0]);
                ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": format!("chloectx-{space}"), "version": env!("CARGO_PKG_VERSION")},
                    "instructions": format!("Memory space {space}: notes that agents and people keep on a ChloeDB cluster. Call ask before a task to get the decisions, rules and facts that apply; call remember when the user decides something, states a preference or a fact worth keeping."),
                }))
            }
            "ping" => ok(json!({})),
            "tools/list" => ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = m["params"]["name"].as_str().unwrap_or("");
                let args = &m["params"]["arguments"];
                match self.mcp_tool(space, grants, name, args) {
                    Ok(text) => ok(json!({"content": [{"type": "text", "text": text}], "isError": false})),
                    Err(Fail(404, e)) if e.starts_with("no tool") => fail(-32602, e),
                    Err(Fail(_, e)) => ok(json!({"content": [{"type": "text", "text": e}], "isError": true})),
                }
            }
            _ => fail(-32601, format!("no method {method}")),
        })
    }

    fn mcp_tool(&self, space: &str, grants: Option<&Grants>, name: &str, args: &Json) -> Result<String, Fail> {
        let table = table_of(space);
        match name {
            "ask" => {
                let q = args["question"].as_str().unwrap_or("");
                let budget = args["budget"].as_u64().map_or(2000, |n| n as usize);
                let a = self.ask(space, q, budget)?;
                let context = a["context"].as_str().unwrap_or("");
                Ok(if context.is_empty() { format!("The space {space} holds nothing for this yet.") } else {
                    let ids: Vec<String> = a["items"].as_array().into_iter().flatten().filter_map(|i| Some(format!("{} ({})", i["id"].as_str()?, i["kind"].as_str().unwrap_or("")))).collect();
                    format!("{context}\n\nNote ids, to replace one with remember: {}", ids.join(", "))
                })
            }
            "remember" => {
                if let Some(g) = grants {
                    g.need(&table, Scope::Write, &format!("write to the space {space}"))?;
                }
                let n = self.remember(space, &json!({"kind": args["kind"], "text": args["text"], "replaces": args["replaces"], "entities": args["entities"], "session": "mcp"}))?;
                Ok(format!("Kept as {}{}.", n["id"].as_str().unwrap_or(""), if n["supersedes"].as_array().is_some_and(|s| !s.is_empty()) { ", replacing the older note (kept as history)" } else { "" }))
            }
            "notes" => {
                let j = self.notes(space, args["kind"].as_str(), args["limit"].as_u64().map_or(50, |n| n as usize), false)?;
                let ns = j["notes"].as_array().cloned().unwrap_or_default();
                Ok(if ns.is_empty() { format!("The space {space} has no current notes.") } else { ns.iter().map(note_line).collect::<Vec<_>>().join("\n") })
            }
            _ => Err(Fail(404, format!("no tool {name}: ask, remember or notes"))),
        }
    }

    /// `POST /v1/spaces/<space>/mcp`: a message or a batch; `None` when
    /// all were notifications (202, no body).
    pub fn mcp(&self, space: &str, grants: Option<&Grants>, body: &Json) -> Result<Option<Json>, Fail> {
        // The space must exist: an agent's typo is not a new space.
        self.space(space)?;
        match body {
            Json::Array(ms) => {
                let out: Vec<Json> = ms.iter().filter_map(|m| self.mcp_one(space, grants, m)).collect();
                Ok((!out.is_empty()).then_some(Json::Array(out)))
            }
            Json::Object(_) => Ok(self.mcp_one(space, grants, body)),
            _ => Err(bad("an MCP message is a JSON-RPC object or a batch of them")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_cover_tables_by_name_and_prefix() {
        let g = Grants::parse("ctx_billing=write, ctx_*=read,sheets_*=admin,bad=nope");
        assert_eq!(g.on("ctx_billing"), Some(Scope::Write));
        assert_eq!(g.on("CTX_other"), Some(Scope::Read));
        assert_eq!(g.on("chloectx_memory"), None);
        assert_eq!(g.on("bad"), None);
        assert_eq!(Grants::parse("*=read").on("anything"), Some(Scope::Read));
    }

    #[test]
    fn a_limited_key_reaches_its_spaces_only() {
        let g = Grants::parse("ctx_billing=write");
        let p = |s: &str| s.trim_matches('/').split('/').map(str::to_string).collect::<Vec<_>>();
        let ok = |m: &str, path: &str| check(&g, m, &p(path).iter().map(|s| s.as_str()).collect::<Vec<_>>()).is_ok();
        assert!(ok("GET", "/v1/spaces") && ok("GET", "/v1/info"));
        assert!(ok("GET", "/v1/spaces/billing/notes") && ok("POST", "/v1/spaces/billing/notes") && ok("POST", "/v1/spaces/billing/mcp") && ok("DELETE", "/v1/spaces/billing/notes/api:1"));
        assert!(!ok("POST", "/v1/spaces/billing/apps") && !ok("POST", "/v1/spaces") && !ok("GET", "/v1/spaces/payroll/notes") && !ok("POST", "/v1/spaces/payroll/mcp"));
        assert!(!ok("GET", "/v1/local") && !ok("POST", "/v1/incidents") && !ok("GET", "/v1/incidents"));
        // The synced projects: the route lists those whose tables the key
        // reads (none here), and reads a project only from a table it may.
        assert!(ok("GET", "/v1/memory"));
        let reader = Grants::parse("ctx_billing=read");
        let path = p("/v1/spaces/billing/notes");
        let path: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
        assert!(check(&reader, "GET", &path).is_ok() && check(&reader, "POST", &path).is_err());
    }
}
