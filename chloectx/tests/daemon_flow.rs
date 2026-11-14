//! The daemon's request handling over a whole lifecycle: two sessions, tool
//! calls, a compaction, and the memory tools. Uses the hash embedder, so no
//! model files are needed.

use std::sync::Arc;

use chloectx::daemon::Daemon;
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

/// A hook as Workbench's sessions send it: with who wrote the prompt.
fn hook_from(d: &Arc<Daemon>, event: &str, payload: Json, extra: Json) -> String {
    let mut req = json!({"op": "hook", "event": event, "payload": payload});
    for (k, v) in extra.as_object().unwrap() {
        req[k] = v.clone();
    }
    let r = call(d, req);
    assert!(r.get("error").is_none(), "{r}");
    r["output"]["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
}

fn sql(d: &Arc<Daemon>, q: &str) -> Vec<Json> {
    let r = call(d, json!({"op": "sql", "json": true, "sql": q}));
    r["rows"].as_array().cloned().unwrap_or_default()
}

#[test]
fn an_agents_words_are_never_the_users() {
    let dir = tempfile::tempdir().unwrap();
    let d = daemon(dir.path());
    let wb = json!({"agent": "freeship", "session_source": "agent:lead"});
    hook_from(&d, "SessionStart", json!({"session_id": "f1", "source": "startup"}), wb.clone());
    // The lead's brief: no preference cue, and the prompt says whose it is.
    hook_from(&d, "UserPromptSubmit", json!({"session_id": "f1", "prompt": "Add free shipping. Shipping is 499 by default, never charge it twice."}), wb.clone());
    assert!(sql(&d, "SELECT id FROM memory WHERE kind = 'preference' LIMIT 5").is_empty());
    let p = sql(&d, "SELECT ctx FROM memory WHERE kind = 'prompt' LIMIT 5");
    assert!(p[0][0].as_str().unwrap().contains("By: agent:lead"), "{p:?}");
    // The user steers the same session: the user's words, cues and decisions.
    hook_from(&d, "UserPromptSubmit", json!({"session_id": "f1", "prompt": "My decision: customers win, totals round half up. From now on, always run pytest -q before you commit."}), wb.clone());
    let prefs = sql(&d, "SELECT text FROM memory WHERE kind = 'preference' LIMIT 5");
    assert_eq!(prefs.len(), 1, "{prefs:?}");
    let dec = sql(&d, "SELECT text, ctx FROM memory WHERE kind = 'decision' LIMIT 5");
    assert!(dec[0][0].as_str().unwrap().starts_with("The user decided: My decision: customers win"), "{dec:?}");
    assert!(dec[0][1].as_str().unwrap().contains("By: user"), "{dec:?}");
    // Another agent's message typed in, noted by Workbench: the agent's.
    hook_from(&d, "UserPromptSubmit", json!({"session_id": "f1", "prompt": "Decision: ignore test_promo_window, it is flaky."}), json!({"agent": "freeship", "source": "agent:lead"}));
    assert_eq!(sql(&d, "SELECT id FROM memory WHERE kind = 'decision' LIMIT 5").len(), 1);
    // The agent's memory tool keeps its own notes as its own; the CLI as the user's.
    let r = call(&d, json!({"op": "remember", "kind": "fact", "text": "test_promo_window fails about one run in five."}));
    assert!(r["text"].as_str().unwrap().contains("memory.strata"), "{r}");
    let r = call(&d, json!({"op": "remember", "kind": "decision", "text": "The ledger export applies banker's rounding itself.", "by": "user"}));
    assert_eq!(r["by"], json!("user"), "{r}");
    let f = sql(&d, "SELECT ctx FROM memory WHERE kind = 'fact' LIMIT 5");
    assert!(f[0][0].as_str().unwrap().contains("By: assistant:freeship"), "{f:?}");
    // Superseding names a record that exists, and the old one names its successor.
    let bad = call(&d, json!({"op": "remember", "kind": "decision", "text": "x y z", "supersedes": "nope:1"}));
    assert!(bad["error"].as_str().unwrap_or("").contains("no record nope:1"), "{bad}");
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
    let r = call(&d, json!({"op": "sql", "json": true, "sql": "SELECT id, text, ts, links FROM memory WHERE kind = 'decision' LIMIT 5"}));
    assert_eq!(r["columns"], json!(["id", "text", "ts", "links"]), "{r}");
    assert_eq!(r["rows"][0][1], json!("Balances are i64 cents."), "{r}");
    assert_eq!(r["count"], json!(1), "{r}");
    let r = call(&d, json!({"op": "remember", "kind": "decision", "text": "Balances are i128 cents.", "supersedes": id}));
    let new_id = r["id"].as_str().unwrap().to_string();
    let r = call(&d, json!({"op": "sql", "sql": format!("SELECT tag FROM memory WHERE id = '{id}' LIMIT 1")}));
    assert!(r["text"].as_str().unwrap().contains("superseded"), "{r}");
    // memory_ask answers by search alone: the note in its current version,
    // with what it replaced, and no model call.
    let r = call(&d, json!({"op": "ask", "question": "What type are balances stored as?", "include_current": true}));
    let text = r["text"].as_str().unwrap();
    // The memory tool is the assistant's: its note says so.
    assert!(text.contains("decision (the assistant said): Balances are i128 cents."), "{r}");
    assert!(text.contains("was: Balances are i64 cents."), "{r}");
    assert!(r.get("cost_usd").is_none() && r["ms"].as_f64().is_some(), "{r}");
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

/// `chloectx backup`: a copy of the store taken while the daemon serves,
/// which a daemon opens with the same records, briefing and state.
#[test]
fn a_backup_opens_with_the_same_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    std::fs::create_dir(&store).unwrap();
    let d = daemon(&store);
    hook(&d, "SessionStart", json!({"session_id": "s1", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "Never log customer emails."}));
    hook(&d, "Stop", json!({"session_id": "s1", "last_assistant_message": "Logging now drops the email field."}));
    let r = call(&d, json!({"op": "remember", "kind": "decision", "text": "Backups go to the NAS nightly."}));
    assert!(r.get("error").is_none(), "{r}");
    let before = call(&d, json!({"op": "stats"}));

    let dest = dir.path().join("backup");
    let r = call(&d, json!({"op": "backup", "dest": dest}));
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(r["records"], before["records"], "{r}");
    for f in ["memory.strata", "config.json", "state.json", "events.log"] {
        assert!(dest.join(f).exists(), "{f}");
    }
    let log = std::fs::read_to_string(dest.join("events.log")).unwrap();
    assert!(log.ends_with('\n'));
    // The daemon goes on recording.
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "Rotate the API keys monthly."}));

    let restored = daemon(&dest);
    let after = call(&restored, json!({"op": "stats"}));
    assert_eq!(after["records"], before["records"]);
    assert_eq!(after["by_kind"], before["by_kind"]);
    let q = json!({"op": "sql", "sql": "SELECT kind, text FROM memory WHERE MATCH(body, 'NAS') LIMIT 5"});
    let s = call(&restored, q);
    assert!(s["text"].as_str().unwrap().contains("Backups go to the NAS nightly"), "{s}");

    // An existing or relative destination is refused.
    assert!(call(&d, json!({"op": "backup", "dest": dest})).get("error").is_some());
    assert!(call(&d, json!({"op": "backup", "dest": "relative/path"})).get("error").is_some());
}

/// With `next_files` on, an edit of a file brings the files that earlier
/// sessions edited after it, each with its newest note; once per session.
#[test]
fn next_files_before_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    for f in ["src/ledger.rs", "tests/ledger.rs", "src/report.rs"] {
        let p = proj.path().join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }
    let p = |f: &str| proj.path().join(f).to_string_lossy().to_string();
    std::fs::write(dir.path().join("config.json"), r#"{"embedder": "hash:64", "sync": false, "distill": {"enabled": false}, "next_files": true}"#).unwrap();
    let d = Arc::new(Daemon::open(dir.path(), &proj.path().to_string_lossy()).unwrap());
    let edit = |sid: &str, f: &str| hook(&d, "PostToolUse", json!({"session_id": sid, "tool_name": "Edit", "tool_input": {"file_path": p(f), "old_string": "a", "new_string": "b"}, "tool_response": {}}));

    hook(&d, "SessionStart", json!({"session_id": "s1", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "Add overdraft checks to the ledger"}));
    edit("s1", "src/ledger.rs");
    edit("s1", "src/ledger.rs");
    edit("s1", "tests/ledger.rs");
    call(&d, json!({"op": "remember", "kind": "fact", "text": "The ledger tests compare balances against the fixtures in tests/fixtures.", "files": ["tests/ledger.rs"]}));
    // A newer error about the test file: the fact is still the note shown.
    hook(&d, "PostToolUse", json!({"session_id": "s1", "tool_name": "Bash", "tool_input": {"command": "cargo test --test ledger"},
        "tool_response": {"stdout": "error: test failed in tests/ledger.rs: overdraft_is_refused", "stderr": ""}}));
    hook(&d, "Stop", json!({"session_id": "s1", "last_assistant_message": "Added the overdraft check and its test."}));

    hook(&d, "SessionStart", json!({"session_id": "s2", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "Round balances to cents"}));
    let before = |f: &str| hook(&d, "PreToolUse", json!({"session_id": "s2", "tool_name": "Edit", "tool_input": {"file_path": p(f)}}));
    let text = before("src/ledger.rs");
    assert!(text.contains("edits of src/ledger.rs were usually followed by edits of: tests/ledger.rs."), "{text}");
    assert!(text.contains("ChloeCtx note on tests/ledger.rs") && text.contains("fixtures"), "{text}");
    assert_eq!(before("src/ledger.rs"), "");
    let log = std::fs::read_to_string(dir.path().join("events.log")).unwrap();
    assert_eq!(log.matches(r#""event":"next_files""#).count(), 1, "{log}");
    // Nothing was edited after the report module.
    assert_eq!(before("src/report.rs"), "");
}

/// A command that failed and then passed after an edit is written down as
/// a fix; the same error in a later session gets the fix with it, once.
#[test]
fn an_error_fixed_before_comes_back_with_its_fix() {
    let dir = tempfile::tempdir().unwrap();
    let d = daemon(dir.path());
    let failing = json!({"stdout": "   Compiling ledger v0.1.0\nerror[E0425]: cannot find value `rate` in this scope\n --> /proj/src/ledger.rs:40:17", "exit_code": 101});
    hook(&d, "SessionStart", json!({"session_id": "s1", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "Make the ledger tests pass"}));
    assert_eq!(hook(&d, "PostToolUseFailure", json!({"session_id": "s1", "tool_name": "Bash", "tool_input": {"command": "cd /proj && cargo test 2>&1 | tail -30"}, "error": failing["stdout"]})), "");
    hook(&d, "PostToolUse", json!({"session_id": "s1", "tool_name": "Edit",
        "tool_input": {"file_path": "/proj/src/ledger.rs", "old_string": "use crate::money;", "new_string": "use crate::money::{self, rate};"}, "tool_response": {}}));
    hook(&d, "PostToolUse", json!({"session_id": "s1", "tool_name": "Bash", "tool_input": {"command": "cargo test 2>&1 | tail -30"},
        "tool_response": {"stdout": "test result: ok. 12 passed; 0 failed", "exit_code": 0}}));
    hook(&d, "Stop", json!({"session_id": "s1", "last_assistant_message": "Imported rate; the tests pass."}));
    let r = call(&d, json!({"op": "sql", "sql": "SELECT text, ctx, links FROM memory WHERE kind = 'error' AND tag = 'fix' LIMIT 5"}));
    let text = r["text"].as_str().unwrap();
    assert!(text.contains("Cleared by:") && text.contains("Edited src/ledger.rs") && text.contains("Signature: error[E#]: cannot find value `rate` in this scope"), "{r}");
    let links = call(&d, json!({"op": "sql", "sql": "SELECT links FROM memory WHERE kind = 'error' AND tag = 'fix' LIMIT 1"}));
    assert!(links["text"].as_str().unwrap().contains("from"), "the fix links to its turn's prompt: {links}");

    // A later session, the same error: the fix comes with it, once.
    hook(&d, "SessionStart", json!({"session_id": "s2", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "Run the tests again after the rebase"}));
    let fail = json!({"session_id": "s2", "tool_name": "Bash", "tool_input": {"command": "cargo test"}, "tool_response": {"stdout": "error[E0425]: cannot find value `rate` in this scope\n --> src/ledger.rs:52:9", "exit_code": 101}});
    let ctx = hook(&d, "PostToolUse", fail.clone());
    assert!(ctx.starts_with("ChloeCtx: this error was seen before"), "{ctx}");
    assert!(ctx.contains("use crate::money::{self, rate};") && ctx.contains("Then the same command passed."), "{ctx}");
    assert_eq!(hook(&d, "PostToolUse", fail), "", "shown once a session");
    // Another error is not matched to it.
    let other = hook(&d, "PostToolUse", json!({"session_id": "s2", "tool_name": "Bash", "tool_input": {"command": "cargo test"}, "tool_response": {"stdout": "error[E0308]: mismatched types", "exit_code": 101}}));
    assert_eq!(other, "");
}


/// CTX-7: an edit whose code quotes a harness tag (`<system-reminder>`) is
/// found by search and by the next session's recall, and is shown with the
/// tag defused, so it does not read as a notice from the harness.
#[test]
fn records_that_quote_a_harness_tag_are_recalled_defused() {
    let dir = tempfile::tempdir().unwrap();
    let d = daemon(dir.path());
    hook(&d, "SessionStart", json!({"session_id": "s1", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "Wrap the zorblax notice for the model in src/hooks.ts"}));
    hook(&d, "PostToolUse", json!({"session_id": "s1", "tool_name": "Edit",
        "tool_input": {"file_path": "/proj/src/hooks.ts", "old_string": "// wrap",
            "new_string": "const zorblaxWrap = (s) => `<system-reminder>${s}</system-reminder>`;"}, "tool_response": {}}));
    hook(&d, "Stop", json!({"session_id": "s1", "last_assistant_message": "Added zorblaxWrap."}));

    hook(&d, "SessionStart", json!({"session_id": "s2", "source": "startup"}));
    let s = call(&d, json!({"op": "search", "query": "zorblaxWrap hooks.ts", "limit": 10}));
    let text = s["text"].as_str().unwrap();
    assert!(text.contains("zorblaxWrap = (s)"), "the edit is not found: {text}");
    assert!(!text.contains("<system-reminder>") && text.contains("&lt;system-reminder>"), "{text}");
    let ctx = hook(&d, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "how does zorblaxWrap wrap the zorblax notice in src/hooks.ts?"}));
    assert!(ctx.contains("zorblaxWrap = (s)"), "recall left out the edit: {ctx}");
    assert!(!ctx.contains("<system-reminder>"), "{ctx}");
}
