//! One store for a repository's worktrees, the events the hooks record and
//! never show (notifications, sub-agents' ends, compactions' starts), and
//! `chloectx report` over what was recorded. The hash embedder, so no
//! model files are needed; no git either (the worktree's files are written
//! as `git worktree add` leaves them).

use std::path::{Path, PathBuf};
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

/// A hook as the client sends it: the folder it runs in travels as `worktree`.
fn hook(d: &Arc<Daemon>, tree: &Path, event: &str, payload: Json) -> String {
    let r = call(d, json!({"op": "hook", "event": event, "payload": payload, "worktree": tree}));
    assert!(r.get("error").is_none(), "{r}");
    r["output"]["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
}

fn sql(d: &Arc<Daemon>, q: &str) -> String {
    call(d, json!({"op": "sql", "sql": q}))["text"].as_str().unwrap_or("").to_string()
}

/// `main/` with its `.git/`, and `wt/`, a linked worktree of it.
fn repo(t: &Path) -> (PathBuf, PathBuf) {
    let main = t.join("main");
    let wt = t.join("wt");
    std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
    std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(main.join(".git/worktrees/wt/HEAD"), "ref: refs/heads/feature\n").unwrap();
    std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
    for d in [&main, &wt] {
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/billing.py"), "def total(cents):\n    return round_half_up(cents)\n").unwrap();
    }
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/wt").display())).unwrap();
    (main.canonicalize().unwrap(), wt.canonicalize().unwrap())
}

fn open(store: &Path, project: &Path) -> Arc<Daemon> {
    std::fs::write(store.join("config.json"), r#"{"embedder": "hash:64", "sync": false, "distill": {"enabled": false}}"#).unwrap();
    Arc::new(Daemon::open(store, &project.to_string_lossy()).unwrap())
}

#[test]
fn worktrees_share_a_store_and_check_notes_against_their_own_files() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("CHLOECTX_HOME", home.path());
    let t = tempfile::tempdir().unwrap();
    let (main, wt) = repo(t.path());

    // Hooks in either folder reach one store, the main checkout's, which
    // keeps the name it had before stores were shared.
    assert_eq!(chloectx::paths::store_dir(&wt), chloectx::paths::store_dir(&main));
    assert_eq!(chloectx::paths::store_root(&wt), main);
    assert!(chloectx::paths::store_dir(&main).ends_with(chloectx::paths::project_slug(&main)));

    let store = tempfile::tempdir().unwrap();
    let d = open(store.path(), &main);
    // A session in the worktree: its paths are relative to the worktree.
    hook(&d, &wt, "SessionStart", json!({"session_id": "wt-session", "source": "startup"}));
    hook(&d, &wt, "UserPromptSubmit", json!({"session_id": "wt-session", "prompt": "Round invoice totals half up"}));
    hook(&d, &wt, "PostToolUse", json!({"session_id": "wt-session", "tool_name": "Edit",
        "tool_input": {"file_path": wt.join("src/billing.py"), "old_string": "round(", "new_string": "round_half_up("}, "tool_response": {}}));
    let files = sql(&d, "SELECT files FROM memory WHERE kind = 'tool_call' LIMIT 5");
    assert!(files.contains("src/billing.py") && !files.contains(&*wt.to_string_lossy()), "{files}");
    // A note written from the worktree is anchored to the worktree's file.
    let r = call(&d, json!({"op": "remember", "kind": "decision", "text": "Invoice totals use round_half_up in src/billing.py.", "files": ["src/billing.py"], "worktree": wt}));
    assert!(r.get("error").is_none(), "{r}");
    hook(&d, &wt, "Stop", json!({"session_id": "wt-session", "last_assistant_message": "Totals now round half up."}));

    // The worktree's copy goes away; the main checkout's stays.
    std::fs::remove_file(wt.join("src/billing.py")).unwrap();
    let from_wt = call(&d, json!({"op": "briefing", "session": "", "worktree": wt}))["text"].as_str().unwrap().to_string();
    let from_main = call(&d, json!({"op": "briefing", "session": "", "worktree": main}))["text"].as_str().unwrap().to_string();
    assert!(from_wt.contains("round_half_up") && from_main.contains("round_half_up"), "{from_wt}\n---\n{from_main}");
    assert!(from_wt.contains("may be out of date") || from_wt.contains("no longer exist"), "checked against the worktree:\n{from_wt}");
    assert!(!from_main.contains("no longer exist"), "checked against the main checkout:\n{from_main}");

    // A session in the main checkout sees the worktree's records, and is
    // checked against its own files at its start.
    let brief = hook(&d, &main, "SessionStart", json!({"session_id": "main-session", "source": "startup"}));
    assert!(brief.contains("round_half_up") && !brief.contains("no longer exist"), "{brief}");

    // Where each session ran is kept with it, and logged with its branch.
    let state: Json = { d.persist(); serde_json::from_str(&std::fs::read_to_string(store.path().join("state.json")).unwrap()).unwrap() };
    assert_eq!(state["sessions"]["wt-session"]["worktree"], json!(wt));
    assert_eq!(state["sessions"]["main-session"]["worktree"], json!(main));
    let log = std::fs::read_to_string(store.path().join("events.log")).unwrap();
    assert!(log.lines().any(|l| l.contains("\"event\":\"worktree\"") && l.contains("\"branch\":\"feature\"")), "{log}");

    // The old behaviour on request: a store per worktree.
    std::env::set_var("CHLOECTX_PER_WORKTREE", "1");
    assert_ne!(chloectx::paths::store_dir(&wt), chloectx::paths::store_dir(&main));
    std::env::remove_var("CHLOECTX_PER_WORKTREE");
}

#[test]
fn events_are_recorded_never_shown_and_reported() {
    let t = tempfile::tempdir().unwrap();
    let proj = t.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let store = tempfile::tempdir().unwrap();
    let d = open(store.path(), &proj);

    // A transcript holding the tool calls, so that the daemon can time them.
    let transcript = t.path().join("t.jsonl");
    let ago = |s: i64| {
        let ts = chloectx::now() - s;
        format!("{}Z", chloectx::recall::fmt_time(ts).replace(" UTC", "").replace(' ', "T") + &format!(":{:02}", ts % 60))
    };
    let mut lines = Vec::new();
    for (i, s) in [30i64, 20, 10].iter().enumerate() {
        lines.push(json!({"type": "assistant", "timestamp": ago(*s), "message": {"content": [{"type": "tool_use", "id": format!("toolu_{i}"), "name": "Bash", "input": {"command": "cargo test"}}]}}).to_string());
    }
    std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();

    hook(&d, &proj, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "Make the tests pass"}));
    for i in 0..3 {
        hook(&d, &proj, "PostToolUse", json!({"session_id": "s1", "tool_name": "Bash", "tool_use_id": format!("toolu_{i}"), "transcript_path": transcript,
            "tool_input": {"command": "cargo test"}, "tool_response": {"stdout": "error[E0425]: cannot find value `ledger_total` in this scope", "stderr": ""}}));
    }
    for _ in 0..3 {
        assert_eq!(hook(&d, &proj, "Notification", json!({"session_id": "s1", "message": "Claude needs your permission to use Bash", "notification_type": "permission_prompt"})), "");
    }
    hook(&d, &proj, "Notification", json!({"session_id": "s1", "message": "Claude is waiting for your input"}));
    let r = call(&d, json!({"op": "hook", "event": "SubagentStop", "payload": {"session_id": "s1", "agent_type": "Explore", "stop_hook_active": false}, "worktree": proj}));
    assert!(r["output"].is_null(), "a sub-agent is never held back: {r}");
    hook(&d, &proj, "PreCompact", json!({"session_id": "s1", "trigger": "auto"}));
    hook(&d, &proj, "PostCompact", json!({"session_id": "s1", "compact_summary": "Summary:\n1. Primary Request and Intent:\n   make the tests pass"}));
    hook(&d, &proj, "Stop", json!({"session_id": "s1", "last_assistant_message": "The tests pass now."}));

    let ev = sql(&d, "SELECT tag, text FROM memory WHERE kind = 'event' LIMIT 20");
    for tag in ["notification:permission_prompt", "notification:idle_prompt", "subagent_stop", "compact_start"] {
        assert!(ev.contains(tag), "{tag} in\n{ev}");
    }
    // The calls were timed from the transcript.
    let took = sql(&d, "SELECT ctx FROM memory WHERE kind = 'tool_call' LIMIT 5");
    assert!(took.contains("Took: "), "{took}");

    // Never shown: not by search, the briefing, a prompt's recall or a timeline.
    let s = call(&d, json!({"op": "search", "query": "Claude needs your permission to use Bash", "limit": 20, "include_current": true}));
    assert!(!s["text"].as_str().unwrap().contains("Notification"), "{s}");
    let s = call(&d, json!({"op": "search", "query": "permission", "limit": 20, "include_current": true, "kinds": ["event"]}));
    assert!(s["text"].as_str().unwrap().contains("Notification"), "asked for by kind: {s}");
    let brief = hook(&d, &proj, "SessionStart", json!({"session_id": "s2", "source": "startup"}));
    assert!(!brief.contains("Notification") && !brief.contains("Sub-agent"), "{brief}");
    let ctx = hook(&d, &proj, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "Why did Claude need your permission to use Bash, and which sub-agent finished?"}));
    assert!(!ctx.contains("Notification:") && !ctx.contains("Sub-agent Explore"), "{ctx}");
    let first = sql(&d, "SELECT id FROM memory WHERE kind = 'prompt' AND session = 's1' LIMIT 1");
    let id = first.lines().find_map(|l| l.split_whitespace().find(|w| w.starts_with("s1:"))).unwrap().to_string();
    let tl = call(&d, json!({"op": "timeline", "id": id, "before": 0, "after": 40}))["text"].as_str().unwrap().to_string();
    assert!(tl.contains("cargo test") && !tl.contains("Notification"), "{tl}");

    // The report: it reads, and finds the loop and the permission prompts.
    let before = sql(&d, "SELECT COUNT(*) FROM memory LIMIT 1");
    let r = call(&d, json!({"op": "report", "days": 7, "session": "s1"}));
    assert!(r.get("error").is_none(), "{r}");
    let rep = &r["report"];
    assert_eq!(rep["sessions"].as_array().unwrap().len(), 1, "{rep}");
    assert_eq!(rep["permission_prompts"], json!(3));
    assert_eq!(rep["subagents"], json!(1));
    assert_eq!(rep["compactions"], json!(1));
    assert_eq!(rep["commands"][0]["command"], json!("cargo test"));
    assert_eq!(rep["commands"][0]["runs"], json!(3));
    assert_eq!(rep["errors"][0]["count"], json!(3), "{rep}");
    assert!(rep["timed_calls"].as_u64().unwrap() >= 1, "{rep}");
    let text = r["text"].as_str().unwrap();
    assert!(text.contains("whole seconds") && text.contains("Suggestions"), "{text}");
    assert!(text.contains("permission"), "{text}");
    assert_eq!(sql(&d, "SELECT COUNT(*) FROM memory LIMIT 1"), before, "the report writes nothing");
}
