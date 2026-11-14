//! `chloectx mcp`: a Model Context Protocol server over stdio (newline-
//! delimited JSON-RPC 2.0) exposing the memory tools. Each call is forwarded
//! to the project's daemon, on its own thread: a quick search is answered
//! while a slow `memory_ask` (with `deep`) is still running.

use std::io::{BufRead, Write};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::{daemon, paths};

const INSTRUCTIONS: &str = "ChloeCtx is this project's long-term memory: every earlier prompt, answer, tool call, error and compaction summary, across sessions. \
Relevant records are injected automatically when a prompt clearly matches them; these tools are for everything else. \
memory_ask is the way to answer a question about earlier work (what was decided, why, what was recommended, which error, which numbers): it searches notes, rules, replies and messages at once and returns them packed for the question, in milliseconds. \
memory_search is one quick hybrid search. \
memory_timeline shows what happened around a record id. memory_sql answers structured questions (counts, by file, by kind, by time). \
memory_remember stores a decision, preference, fact or open task worth keeping beyond this session, ideally one sentence that will make sense later.";

fn tools() -> Json {
    json!([
        {
            "name": "memory_ask",
            "description": "Answer a question about this project's earlier work from memory. Several searches run at once and their results come back packed for the question, with no model call: the rules that bear on it, notes in their current versions with what they replaced (oldest first), what the assistant replied in the exchanges closest to it, and excerpts of the messages. Answer from them; prefer the latest of two values. Use it before design decisions and for anything earlier work may have settled. With deep, a model plans the searches and picks the records instead (several seconds).",
            "inputSchema": {"type": "object", "properties": {
                "question": {"type": "string", "minLength": 1, "description": "The question or task, in natural language."},
                "deep": {"type": "boolean", "default": false, "description": "Let a model rewrite the question into searches and pick the records, with a short answer citing record ids."},
                "model": {"type": "string", "description": "With deep: model for planning and ranking (default: the configured one, usually haiku)."},
                "include_current": {"type": "boolean", "default": false}
            }, "required": ["question"]}
        },
        {
            "name": "memory_search",
            "description": "Search this project's history (earlier prompts, answers, tool calls and results, errors, compaction summaries, remembered decisions) with hybrid BM25 + vector search. Returns record ids with time, session, turn, kind and a snippet. Records already in the current context window are excluded unless include_current is true.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "minLength": 1, "description": "What to look for, in natural language or keywords."},
                "kinds": {"type": "array", "items": {"type": "string", "enum": ["prompt", "answer", "tool_call", "tool_result", "error", "episode", "summary", "decision", "preference", "fact", "todo", "note"]}, "description": "Only these record kinds."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 8},
                "since_days": {"type": "number", "minimum": 0, "description": "Only records from the last N days."},
                "include_current": {"type": "boolean", "default": false}
            }, "required": ["query"]}
        },
        {
            "name": "memory_timeline",
            "description": "The records just before and after a record id in its session, with full text: what was asked, what was run, what came out.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string", "minLength": 1}, "before": {"type": "integer", "minimum": 0, "maximum": 100, "default": 8}, "after": {"type": "integer", "minimum": 0, "maximum": 100, "default": 8}
            }, "required": ["id"]}
        },
        {
            "name": "memory_sql",
            "description": "Read-only SQL over the table memory(id, session, kind, tag, turn, seq, ts, importance, files, text, ctx, links). Full-text search uses MATCH(body, 'a AND b') or ORDER BY bm25(body, 'words') DESC; paths use MATCH(files, 'exec AND rs'); ts is Unix seconds; kind is one of prompt, answer, tool_call, tool_result, error, episode, summary, decision, preference, fact, todo, note, entity (a thing notes are about, id 'e:<name>' or 'f:<path>'); links holds a note's edges (about an entity, from its turn, supersedes an older note) and REACHABLE(links, 'e:count sheet', hops => 1, dir => 'in') finds the records about an entity; tag holds the tool name or summary section. Supports WHERE with AND/OR/NOT/IN/BETWEEN, ORDER BY a column or score, GROUP BY with COUNT(*), LIMIT. One SELECT per call, without llm() or cross_encoder(); it stops after 2 seconds and returns at most 1000 rows (100 shown). Example: SELECT ts, text FROM memory WHERE kind = 'error' AND MATCH(files, 'planner') ORDER BY ts DESC LIMIT 5",
            "inputSchema": {"type": "object", "properties": {"sql": {"type": "string", "minLength": 1}}, "required": ["sql"]}
        },
        {
            "name": "memory_remember",
            "description": "Store a durable decision, preference, fact, open task or note for future sessions. Use a self-contained sentence (what, why, where). Use supersedes with an older record id when this replaces it: that record is then marked superseded (an id that is not in memory is refused, and nothing is stored).",
            "inputSchema": {"type": "object", "properties": {
                "kind": {"type": "string", "enum": ["decision", "preference", "fact", "todo", "note"]},
                "text": {"type": "string", "minLength": 1},
                "files": {"type": "array", "items": {"type": "string"}},
                "supersedes": {"type": "string", "minLength": 1}
            }, "required": ["kind", "text"]}
        },
        {
            "name": "memory_forget",
            "description": "Delete a record that is wrong or must not be kept (for example a leaked secret), for good: it cannot be undone. The record is removed from the store and scrubbed from the receipts that showed it, and the automatic snapshots (which hold it) are dropped. It removes that one record, not its turn: the prompt, answer and tool records around it stay, so forget each that holds what must go (memory_timeline shows them). An id that is not in memory is an error.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string", "minLength": 1}}, "required": ["id"]}
        }
    ])
}

fn call(dir: &std::path::Path, project: &str, name: &str, args: &Json) -> (String, bool) {
    let op = match name {
        "memory_ask" => "ask",
        "memory_search" => "search",
        "memory_timeline" => "timeline",
        "memory_sql" => "sql",
        "memory_remember" => "remember",
        "memory_forget" => "forget",
        _ => return (format!("unknown tool {name}"), true),
    };
    let mut req = args.clone();
    if !req.is_object() {
        req = json!({});
    }
    req["op"] = json!(op);
    let wait = Duration::from_secs(if op == "ask" { 300 } else { 20 });
    match daemon::request(dir, project, &req, wait) {
        Ok(r) => match r.get("error") {
            Some(e) => (e.as_str().unwrap_or("error").to_string(), true),
            None => (r.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(), failed(op, &r)),
        },
        Err(e) => (e.to_string(), true),
    }
}

/// Whether a daemon's answer to `op` says it did not do what was asked: a
/// forget that deleted nothing ("No record nope:1.").
fn failed(op: &str, r: &Json) -> bool {
    op == "forget" && r["deleted"].as_array().is_some_and(|d| d.is_empty())
}

/// Checks a call's arguments against the input schema its tool publishes
/// (the parts of JSON Schema the tools use: types, `required`, `enum`,
/// `minLength`, `minimum`, `maximum`, array `items`, and no property it
/// does not name). An agent's mistake is said, not run as something else:
/// a search without its query, a kind that does not exist, a limit past
/// the most. A tool not listed is left to the caller.
pub fn check_args(tools: &Json, name: &str, args: &Json) -> std::result::Result<(), String> {
    let Some(schema) = tools.as_array().and_then(|ts| ts.iter().find(|t| t["name"] == name)).map(|t| &t["inputSchema"]) else { return Ok(()) };
    let empty = serde_json::Map::new();
    let args = match args {
        Json::Object(m) => m,
        Json::Null => &empty,
        _ => return Err(format!("{name}: the arguments are a JSON object")),
    };
    let props = schema["properties"].as_object().cloned().unwrap_or_default();
    for key in schema["required"].as_array().into_iter().flatten().filter_map(|k| k.as_str()) {
        if args.get(key).is_none_or(|v| v.is_null()) {
            return Err(format!("{name}: {key} is required"));
        }
    }
    for (key, v) in args {
        let Some(p) = props.get(key) else {
            let mut known: Vec<&str> = props.keys().map(|k| k.as_str()).collect();
            known.sort();
            return Err(format!("{name} has no argument {key} (it takes {})", known.join(", ")));
        };
        if !v.is_null() {
            check_value(p, v).map_err(|e| format!("{name}: {key} {e}"))?;
        }
    }
    Ok(())
}

fn check_value(p: &Json, v: &Json) -> std::result::Result<(), String> {
    let ok = match p["type"].as_str().unwrap_or("") {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => true,
    };
    if !ok {
        return Err(format!("is {} {}, not {v}", if p["type"] == "integer" || p["type"] == "array" || p["type"] == "object" { "an" } else { "a" }, p["type"].as_str().unwrap_or("value")));
    }
    if let Some(e) = p["enum"].as_array() {
        if !e.contains(v) {
            return Err(format!("is one of {}, not {v}", e.iter().map(|x| x.as_str().map_or_else(|| x.to_string(), String::from)).collect::<Vec<_>>().join(", ")));
        }
    }
    if let (Some(min), Some(s)) = (p["minLength"].as_u64(), v.as_str()) {
        if (s.trim().chars().count() as u64) < min {
            return Err("is empty".into());
        }
    }
    if let Some(x) = v.as_f64() {
        if let Some(min) = p["minimum"].as_f64().filter(|m| x < *m) {
            return Err(format!("is at least {min}, not {v}"));
        }
        if let Some(max) = p["maximum"].as_f64().filter(|m| x > *m) {
            return Err(format!("is at most {max}, not {v}"));
        }
    }
    if let Some(items) = v.as_array().filter(|_| p["items"].is_object()) {
        for x in items {
            check_value(&p["items"], x).map_err(|e| format!("item {e}"))?;
        }
    }
    Ok(())
}

pub fn run() -> i32 {
    let project = paths::project_dir(None);
    let dir = paths::store_dir(&project);
    let proj = project.to_string_lossy().to_string();
    serve(std::io::stdin().lock(), &Mutex::new(std::io::stdout()), |name, args| call(&dir, &proj, name, args));
    0
}

/// Answers the requests read from `input` on `out`. Tool calls run side by
/// side, each on its own thread, and their responses (which carry their
/// request's id) are written as they finish; at the end of the input the
/// calls still running are waited for.
pub fn serve<W: Write + Send>(input: impl BufRead, out: &Mutex<W>, call: impl Fn(&str, &Json) -> (String, bool) + Sync) {
    serve_with(input, out, "chloectx", INSTRUCTIONS, &tools(), call)
}

/// [`serve`] for another server: its name, instructions and tools
/// (`chloectx mount` serves capsules with it).
pub fn serve_with<W: Write + Send>(input: impl BufRead, out: &Mutex<W>, server: &str, instructions: &str, tools: &Json, call: impl Fn(&str, &Json) -> (String, bool) + Sync) {
    let call = &call;
    std::thread::scope(|sc| {
        for line in input.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let msg = match serde_json::from_str::<Json>(&line) {
                Ok(m) => m,
                // JSON-RPC's parse error, with no id: there was none to read.
                Err(e) => {
                    if !write_line(out, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})) {
                        break;
                    }
                    continue;
                }
            };
            let id = msg.get("id").cloned();
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let result = match method {
                "initialize" => {
                    let version = msg.pointer("/params/protocolVersion").and_then(|v| v.as_str()).unwrap_or("2025-06-18");
                    Some(json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": server, "version": env!("CARGO_PKG_VERSION")},
                        "instructions": instructions
                    }))
                }
                "ping" => Some(json!({})),
                "tools/list" => Some(json!({"tools": tools})),
                "tools/call" => {
                    let name = msg.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                    let checked = check_args(tools, &name, &args);
                    sc.spawn(move || {
                        let (text, is_error) = match checked {
                            Ok(()) => call(&name, &args),
                            Err(e) => (e, true),
                        };
                        respond(out, id, method_result(Some(json!({"content": [{"type": "text", "text": text}], "isError": is_error})), "tools/call"));
                    });
                    continue;
                }
                _ => None,
            };
            if !respond(out, id, method_result(result, method)) {
                break;
            }
        }
    });
}

/// A result, or the error for a method that has none.
fn method_result(result: Option<Json>, method: &str) -> std::result::Result<Json, Json> {
    result.ok_or_else(|| json!({"code": -32601, "message": format!("method not found: {method}")}))
}

/// Writes one response (none for a notification); false when the output is gone.
fn respond<W: Write>(out: &Mutex<W>, id: Option<Json>, result: std::result::Result<Json, Json>) -> bool {
    let Some(id) = id else { return true };
    let resp = match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    };
    write_line(out, &resp)
}

fn write_line<W: Write>(out: &Mutex<W>, msg: &Json) -> bool {
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    writeln!(out, "{msg}").is_ok() && out.flush().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slow call does not hold up a quick one sent after it.
    #[test]
    fn tool_calls_run_side_by_side() {
        let input = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "memory_ask", "arguments": {"question": "why?"}}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "memory_search", "arguments": {"query": "planner"}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "nope"}),
        ]
        .map(|m| m.to_string())
        .join("\n");
        let out = Mutex::new(Vec::new());
        serve(std::io::Cursor::new(input), &out, |name, args| {
            if name == "memory_ask" {
                std::thread::sleep(Duration::from_millis(300));
            }
            (format!("{name} {args}"), false)
        });
        let out = String::from_utf8(out.into_inner().unwrap()).unwrap();
        let resps: Vec<Json> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let ids: Vec<i64> = resps.iter().map(|r| r["id"].as_i64().unwrap()).collect();
        assert_eq!(ids.len(), 4, "{out}");
        assert_eq!(ids.last(), Some(&1), "the slow call answers last: {out}");
        assert!(resps.iter().any(|r| r["id"] == 2 && r["result"]["content"][0]["text"].as_str().unwrap().starts_with("memory_search")));
        assert!(resps.iter().any(|r| r["id"] == 4 && r["error"]["code"] == -32601));
    }

    /// CTX-14: what the schemas forbid is refused with the reason, before
    /// the daemon is asked; a line that is not JSON gets JSON-RPC's parse
    /// error; a forget that deleted nothing is an error.
    #[test]
    fn invalid_input_is_refused_and_said() {
        let t = tools();
        let bad = |name: &str, args: Json| check_args(&t, name, &args).expect_err(&format!("{name} {args}"));
        assert!(bad("memory_search", json!({})).contains("query is required"));
        assert!(bad("memory_search", json!({"query": "  "})).contains("query is empty"));
        assert!(bad("memory_search", json!({"query": "x", "kinds": ["bogus"]})).contains("not \"bogus\""));
        assert!(bad("memory_search", json!({"query": "x", "limit": 5000})).contains("at most 50"));
        assert!(bad("memory_search", json!({"query": "x", "limit": "8"})).contains("is an integer"));
        assert!(bad("memory_search", json!({"query": "x", "kind": "decision"})).contains("has no argument kind"));
        assert!(bad("memory_remember", json!({"kind": "bogus", "text": "x"})).contains("is one of decision, preference, fact, todo, note"));
        assert!(bad("memory_remember", json!({"text": "x"})).contains("kind is required"));
        assert!(bad("memory_forget", json!({})).contains("id is required"));
        assert!(bad("memory_timeline", json!({"id": "a:1", "before": -3})).contains("at least 0"));
        assert!(bad("memory_sql", json!("SELECT 1")).contains("JSON object"));
        for (name, args) in [
            ("memory_search", json!({"query": "planner", "kinds": ["decision", "error"], "limit": 50, "since_days": 1.5})),
            ("memory_remember", json!({"kind": "fact", "text": "x", "supersedes": "s:1"})),
            ("memory_ask", json!({"question": "why?", "deep": false})),
            ("memory_forget", json!({"id": "s:1"})),
            ("not_ours", json!({"anything": 1})),
        ] {
            assert_eq!(check_args(&t, name, &args), Ok(()), "{name} {args}");
        }
        assert!(failed("forget", &json!({"text": "No record nope:1.", "deleted": [], "missing": ["nope:1"]})));
        assert!(!failed("forget", &json!({"text": "Deleted s:1.", "deleted": ["s:1"], "missing": []})));
        assert!(!failed("search", &json!({"text": "No matching records."})));

        let input = ["{not json", &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "memory_search", "arguments": {"limit": 5000}}}).to_string()].join("\n");
        let out = Mutex::new(Vec::new());
        let called = std::sync::atomic::AtomicUsize::new(0);
        serve(std::io::Cursor::new(input), &out, |_, _| {
            called.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (String::new(), false)
        });
        let out = String::from_utf8(out.into_inner().unwrap()).unwrap();
        let resps: Vec<Json> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(resps[0]["error"]["code"], -32700, "{out}");
        assert!(resps[0]["id"].is_null());
        assert_eq!(resps[1]["id"], 7);
        assert_eq!(resps[1]["result"]["isError"], true, "{out}");
        assert!(resps[1]["result"]["content"][0]["text"].as_str().unwrap().contains("query is required"), "{out}");
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 0, "a refused call never reaches the daemon");
    }
}
