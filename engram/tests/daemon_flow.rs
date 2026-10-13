//! The daemon's request handling over a whole lifecycle: two sessions, tool
//! calls, a compaction, and the memory tools. Uses the hash embedder, so no
//! model files are needed.

use std::sync::Arc;

use engram::daemon::Daemon;
use serde_json::{json, Value as Json};

fn call(d: &Arc<Daemon>, req: Json) -> Json {
    let (resp, deferred) = d.handle(&req);
    if let Some(f) = deferred {
        f();
    }
    resp
}

fn hook(d: &Arc<Daemon>, event: &str, payload: Json) -> String {
    let r = call(d, json!({"op": "hook", "event": event, "payload": payload}));
    assert!(r.get("error").is_none(), "{r}");
    r["output"]["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
}

fn daemon(dir: &std::path::Path) -> Arc<Daemon> {
    std::fs::write(dir.join("config.json"), r#"{"embedder": "hash:64", "sync": false, "distill": {"enabled": false}}"#).unwrap();
    Arc::new(Daemon::open(dir, "/proj").unwrap())
}

#[test]
fn sessions_tools_compaction_and_memory_tools() {
    let dir = tempfile::tempdir().unwrap();
    let d = daemon(dir.path());

    // Session one: a preference, a failing build, a fix.
    assert_eq!(hook(&d, "SessionStart", json!({"session_id": "s1", "source": "startup"})), "");
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "Build the ledger module. Always use checked arithmetic for balances."}));
    hook(&d, "PostToolUse", json!({"session_id": "s1", "tool_name": "Bash", "tool_input": {"command": "cargo build"},
        "tool_response": {"stdout": "error[E0308]: mismatched types in ledger balance_after", "stderr": ""}}));
    hook(&d, "PostToolUse", json!({"session_id": "s1", "tool_name": "Edit",
        "tool_input": {"file_path": "/proj/src/ledger.rs", "old_string": "a + b", "new_string": "a.checked_add(b)"}, "tool_response": {}}));
    hook(&d, "Stop", json!({"session_id": "s1", "last_assistant_message": "The ledger now uses checked_add; the E0308 mismatch came from mixing u64 and i64."}));
    let stats = call(&d, json!({"op": "stats"}));
    assert!(stats["records"].as_u64().unwrap() >= 6, "{stats}");
    assert_eq!(stats["by_kind"]["preference"], json!(1));
    assert_eq!(stats["by_kind"]["error"], json!(1));
    assert_eq!(stats["by_kind"]["episode"], json!(1));

    // Within session one, nothing is outside the live window.
    let s = call(&d, json!({"op": "search", "query": "ledger mismatched types", "limit": 5}));
    assert_eq!(s["text"], json!("No matching records."), "{s}");

    // Session two starts with a briefing that carries the preference and the episode.
    let brief = hook(&d, "SessionStart", json!({"session_id": "s2", "source": "startup"}));
    assert!(brief.contains("checked arithmetic"), "{brief}");
    assert!(brief.contains("Turn 1. User asked"), "{brief}");
    // Now session one's records are searchable.
    let s = call(&d, json!({"op": "search", "query": "ledger mismatched types", "limit": 5}));
    assert!(s["text"].as_str().unwrap().contains("E0308"), "{s}");
    // File notes before editing ledger.rs, once per window.
    let notes = hook(&d, "PreToolUse", json!({"session_id": "s2", "tool_name": "Edit", "tool_input": {"file_path": "/proj/src/ledger.rs"}}));
    assert!(notes.contains("src/ledger.rs"), "{notes}");
    assert_eq!(hook(&d, "PreToolUse", json!({"session_id": "s2", "tool_name": "Edit", "tool_input": {"file_path": "/proj/src/ledger.rs"}})), "");

    // Memory tools.
    let r = call(&d, json!({"op": "remember", "kind": "decision", "text": "Balances are i64 cents."}));
    let id = r["id"].as_str().unwrap().to_string();
    let r = call(&d, json!({"op": "sql", "sql": "SELECT id, text FROM memory WHERE kind = 'decision' LIMIT 5"}));
    assert!(r["text"].as_str().unwrap().contains("i64 cents"), "{r}");
    let r = call(&d, json!({"op": "remember", "kind": "decision", "text": "Balances are i128 cents.", "supersedes": id}));
    let new_id = r["id"].as_str().unwrap().to_string();
    let r = call(&d, json!({"op": "sql", "sql": format!("SELECT tag FROM memory WHERE id = '{id}' LIMIT 1")}));
    assert!(r["text"].as_str().unwrap().contains("superseded"), "{r}");
    let t = call(&d, json!({"op": "timeline", "id": "s1:3", "before": 1, "after": 1}));
    assert!(t["text"].as_str().unwrap().contains(">> [s1:3]"), "{t}");
    assert!(call(&d, json!({"op": "sql", "sql": "DELETE FROM memory"})).get("error").is_some());

    // A compaction in session two: summary records join the live window.
    hook(&d, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "Refactor the ledger tests"}));
    hook(&d, "PostCompact", json!({"session_id": "s2", "compact_summary": "Summary:\n1. Primary Request and Intent:\n   refactor ledger tests\n7. Pending Tasks:\n   - port fixtures"}));
    let brief = hook(&d, "SessionStart", json!({"session_id": "s2", "source": "compact"}));
    assert!(brief.contains("Open tasks"), "{brief}");
    let r = call(&d, json!({"op": "forget", "id": new_id}));
    assert!(r["text"].as_str().unwrap().starts_with("Deleted"), "{r}");

    // State survives a restart of the daemon.
    drop(d);
    let d = daemon(dir.path());
    let stats = call(&d, json!({"op": "stats"}));
    assert_eq!(stats["sessions"], json!(2));
    let r = call(&d, json!({"op": "search", "query": "checked_add", "limit": 3, "include_current": true}));
    assert!(r["text"].as_str().unwrap().contains("checked_add"), "{r}");
}

#[test]
fn hook_payloads_never_error() {
    let dir = tempfile::tempdir().unwrap();
    let d = daemon(dir.path());
    for event in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "Stop", "PreCompact", "PostCompact", "SessionEnd", "Notification"] {
        let r = call(&d, json!({"op": "hook", "event": event, "payload": {}}));
        assert!(r.get("error").is_none(), "{event}: {r}");
    }
}

/// Distillation with a stand-in for the model: what a turn announced reaches
/// the next session's briefing and prompts, and is closed once done.
#[test]
fn distilled_knowledge_reaches_later_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.json");
    let script = dir.path().join("fake-model.sh");
    std::fs::write(&script, format!("#!/bin/sh\ncat > /dev/null\ncat '{}'\n", out.display())).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cfg = json!({"embedder": "hash:64", "sync": false, "briefing_chars": 2000, "distill": {"command": script, "model": "m"}});
    std::fs::write(dir.path().join("config.json"), cfg.to_string()).unwrap();
    let d = Arc::new(Daemon::open(dir.path(), "/proj").unwrap());
    let model_says = |lines: &[Json]| {
        let result: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        std::fs::write(&out, json!({"is_error": false, "total_cost_usd": 0.001, "result": result.join("\n")}).to_string()).unwrap();
    };

    model_says(&[
        json!({"kind": "requirement", "text": "The CSV export of the ledger must use semicolons and DD.MM.YYYY dates."}),
        json!({"kind": "decision", "text": "Storage stays JSON because the admins edit the files by hand."}),
    ]);
    hook(&d, "SessionStart", json!({"session_id": "s1", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "Next week we add a CSV export; the bank wants semicolons and DD.MM.YYYY dates."}));
    hook(&d, "Stop", json!({"session_id": "s1", "last_assistant_message": "Noted, the CSV export will follow those rules."}));
    let planned = call(&d, json!({"op": "sql", "sql": "SELECT id FROM memory WHERE kind = 'todo' AND tag = 'planned' LIMIT 5"}));
    let planned_id = planned["text"].as_str().unwrap().lines().find_map(|l| l.split_whitespace().find(|w| w.starts_with("s1:"))).unwrap().to_string();

    let brief = hook(&d, "SessionStart", json!({"session_id": "s2", "source": "startup"}));
    assert!(brief.contains("Planned work") && brief.contains("semicolons"), "{brief}");
    assert!(brief.contains("Decisions:") && brief.contains("admins edit"), "{brief}");
    assert!(brief.contains("search memory"), "{brief}");

    // A third session whose briefing cannot hold the knowledge still gets it
    // with a prompt it bears on.
    let cfg = json!({"embedder": "hash:64", "sync": false, "briefing_chars": 100, "distill": {"command": script, "model": "m"}});
    std::fs::write(dir.path().join("config.json"), cfg.to_string()).unwrap();
    drop(d);
    let d = Arc::new(Daemon::open(dir.path(), "/proj").unwrap());
    let brief = hook(&d, "SessionStart", json!({"session_id": "s3", "source": "startup"}));
    assert!(!brief.contains("semicolons"), "{brief}");
    let ctx = hook(&d, "UserPromptSubmit", json!({"session_id": "s3", "prompt": "Add the CSV export of the ledger."}));
    assert!(ctx.contains("[planned]") && ctx.contains("semicolons"), "{ctx}");

    // The model reports the planned work done: it leaves the briefing.
    model_says(&[json!({"kind": "done", "id": planned_id})]);
    hook(&d, "Stop", json!({"session_id": "s3", "last_assistant_message": "Added export_csv with semicolons and DD.MM.YYYY dates."}));
    let tag = call(&d, json!({"op": "sql", "sql": format!("SELECT tag FROM memory WHERE id = '{planned_id}' LIMIT 1")}));
    assert!(tag["text"].as_str().unwrap().contains("done"), "{tag}");
    let ctx = hook(&d, "UserPromptSubmit", json!({"session_id": "s3", "prompt": "Is the CSV export of the ledger planned?"}));
    assert!(!ctx.contains("[planned]"), "{ctx}");
}
