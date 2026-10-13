//! `engram mcp`: a Model Context Protocol server over stdio (newline-
//! delimited JSON-RPC 2.0) exposing the memory tools. Each call is forwarded
//! to the project's daemon.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::{daemon, paths};

const INSTRUCTIONS: &str = "Engram is this project's long-term memory: every earlier prompt, answer, tool call, error and compaction summary, across sessions. \
Relevant records are injected automatically when a prompt clearly matches them; these tools are for everything else. \
memory_search is the way to answer questions about earlier work (what was decided, why, which error, which numbers) when that history is not in the current context. \
memory_timeline shows what happened around a record id. memory_sql answers structured questions (counts, by file, by kind, by time). \
memory_remember stores a decision, preference, fact or open task worth keeping beyond this session, ideally one sentence that will make sense later.";

fn tools() -> Json {
    json!([
        {
            "name": "memory_search",
            "description": "Search this project's history (earlier prompts, answers, tool calls and results, errors, compaction summaries, remembered decisions) with hybrid BM25 + vector search. Returns record ids with time, session, turn, kind and a snippet. Records already in the current context window are excluded unless include_current is true.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "What to look for, in natural language or keywords."},
                "kinds": {"type": "array", "items": {"type": "string", "enum": ["prompt", "answer", "tool_call", "tool_result", "error", "episode", "summary", "decision", "preference", "fact", "todo", "note"]}, "description": "Only these record kinds."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 8},
                "since_days": {"type": "number", "description": "Only records from the last N days."},
                "include_current": {"type": "boolean", "default": false}
            }, "required": ["query"]}
        },
        {
            "name": "memory_timeline",
            "description": "The records just before and after a record id in its session, with full text: what was asked, what was run, what came out.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"}, "before": {"type": "integer", "default": 8}, "after": {"type": "integer", "default": 8}
            }, "required": ["id"]}
        },
        {
            "name": "memory_sql",
            "description": "Read-only SQL over the table memory(id, session, kind, tag, turn, seq, ts, importance, files, text, ctx, links). Full-text search uses MATCH(body, 'a AND b') or ORDER BY bm25(body, 'words') DESC; paths use MATCH(files, 'exec AND rs'); ts is Unix seconds; kind is one of prompt, answer, tool_call, tool_result, error, episode, summary, decision, preference, fact, todo, note, entity (a thing notes are about, id 'e:<name>' or 'f:<path>'); links holds a note's edges (about an entity, from its turn, supersedes an older note) and REACHABLE(links, 'e:count sheet', hops => 1, dir => 'in') finds the records about an entity; tag holds the tool name or summary section. Supports WHERE with AND/OR/NOT/IN/BETWEEN, ORDER BY a column or score, GROUP BY with COUNT(*), LIMIT. Example: SELECT ts, text FROM memory WHERE kind = 'error' AND MATCH(files, 'planner') ORDER BY ts DESC LIMIT 5",
            "inputSchema": {"type": "object", "properties": {"sql": {"type": "string"}}, "required": ["sql"]}
        },
        {
            "name": "memory_remember",
            "description": "Store a durable decision, preference, fact, open task or note for future sessions. Use a self-contained sentence (what, why, where). Use supersedes with an older record id when this replaces it.",
            "inputSchema": {"type": "object", "properties": {
                "kind": {"type": "string", "enum": ["decision", "preference", "fact", "todo", "note"]},
                "text": {"type": "string"},
                "files": {"type": "array", "items": {"type": "string"}},
                "supersedes": {"type": "string"}
            }, "required": ["kind", "text"]}
        },
        {
            "name": "memory_forget",
            "description": "Delete a record that is wrong or must not be kept (for example a leaked secret).",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}
        }
    ])
}

fn call(dir: &std::path::Path, project: &str, name: &str, args: &Json) -> (String, bool) {
    let op = match name {
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
    match daemon::request(dir, project, &req, Duration::from_secs(20)) {
        Ok(r) => match r.get("error") {
            Some(e) => (e.as_str().unwrap_or("error").to_string(), true),
            None => (r.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(), false),
        },
        Err(e) => (e.to_string(), true),
    }
}

pub fn run() -> i32 {
    let project = paths::project_dir(None);
    let dir = paths::store_dir(&project);
    let proj = project.to_string_lossy().to_string();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Json>(&line) else { continue };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let result = match method {
            "initialize" => {
                let version = msg.pointer("/params/protocolVersion").and_then(|v| v.as_str()).unwrap_or("2025-06-18");
                Some(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "engram", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS
                }))
            }
            "ping" => Some(json!({})),
            "tools/list" => Some(json!({"tools": tools()})),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = call(&dir, &proj, name, &args);
                Some(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            _ => None,
        };
        let Some(id) = id else { continue }; // notifications get no response
        let resp = match result {
            Some(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            None => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}}),
        };
        if writeln!(out, "{resp}").is_err() || out.flush().is_err() {
            break;
        }
    }
    0
}
