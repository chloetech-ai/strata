//! CTX-1: with query understanding on (`async`, the default), the end of a
//! turn neither waits for the background search nor asks the model to go
//! on; what the search finds after the turn is offered at the next prompt.
//! `stop_recheck`, opt-in, waits a few seconds at most and never asks the
//! model to repeat its answer. The model is a stand-in for `claude -p`
//! that picks the first record id it is shown, after a delay.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chloectx::daemon::Daemon;
use serde_json::{json, Value as Json};

/// ChloeCtx's home for this test binary (the model's scratch directory).
fn home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = tempfile::tempdir().unwrap().keep();
        std::env::set_var("CHLOECTX_HOME", &d);
        d
    })
}

/// A ranker that sleeps `delay` seconds, then picks every record id in its
/// prompt with high confidence.
fn fake_ranker(dir: &Path, delay: f64) -> PathBuf {
    let exe = dir.join(format!("claude-{delay}"));
    let script = r#"#!/bin/sh
sleep DELAY
picks=""
for id in $(grep -o '\[[A-Za-z0-9]*:[0-9]*\]' | tr -d '[]' | sort -u); do
  picks="$picks{\\\"id\\\": \\\"$id\\\", \\\"why\\\": \\\"it says where\\\"},"
done
picks=${picks%,}
printf '%s' "{\"is_error\": false, \"result\": \"{\\\"relevant\\\": [$picks], \\\"answer\\\": \\\"see them\\\", \\\"confidence\\\": \\\"high\\\"}\", \"total_cost_usd\": 0}"
"#
    .replace("DELAY", &delay.to_string());
    std::fs::write(&exe, script).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

fn call(d: &Arc<Daemon>, req: Json) -> (Json, Option<chloectx::daemon::Deferred>) {
    d.handle(&req)
}

fn run(d: &Arc<Daemon>, req: Json) -> Json {
    let (r, f) = call(d, req);
    if let Some(f) = f {
        f();
    }
    r
}

fn hook(d: &Arc<Daemon>, event: &str, payload: Json) -> Json {
    let r = run(d, json!({"op": "hook", "event": event, "payload": payload}));
    assert!(r.get("error").is_none(), "{r}");
    r
}

fn ctx(r: &Json) -> String {
    r["output"]["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
}

fn daemon(store: &Path, exe: &Path, deep: Json) -> Arc<Daemon> {
    let cfg = json!({"embedder": "hash:64", "sync": false, "distill": {"enabled": false, "command": exe}, "deep": deep});
    std::fs::write(store.join("config.json"), cfg.to_string()).unwrap();
    let d = Arc::new(Daemon::open(store, "/proj").unwrap());
    // An earlier session the search can find.
    hook(&d, "SessionStart", json!({"session_id": "old", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "old", "prompt": "Where do we cache the exchange rates?"}));
    for t in [
        "Exchange rates are cached in Redis under fx:rates for 15 minutes.",
        "The fx refresher job runs every 10 minutes from cron.",
        "Invoices use integer cents throughout.",
    ] {
        run(&d, json!({"op": "remember", "kind": "fact", "text": t}));
    }
    // Tool output, more than recall shows at a prompt.
    for i in 0..30 {
        hook(&d, "PostToolUse", json!({"session_id": "old", "tool_name": "Bash", "tool_input": {"command": format!("grep -rn fx_rate_{i} src/")},
            "tool_response": {"stdout": format!("src/fx/cache_{i}.rs: exchange rates cache entry {i} expires after 15 minutes"), "stderr": ""}}));
    }
    hook(&d, "Stop", json!({"session_id": "old", "last_assistant_message": "In Redis, 15 minutes."}));
    d
}

/// The end of a turn that comes while the search still runs returns at
/// once, without a block; the search's result comes with the next prompt.
#[test]
fn the_end_of_a_turn_does_not_wait_and_never_blocks() {
    home();
    let t = tempfile::tempdir().unwrap();
    let exe = fake_ranker(t.path(), 1.5);
    let store = tempfile::tempdir().unwrap();
    let d = daemon(store.path(), &exe, json!({"mode": "async", "planner": "none", "min_records": 1, "min_words": 2}));

    hook(&d, "SessionStart", json!({"session_id": "s", "source": "startup"}));
    // The prompt's background work (the search among it) runs on a thread
    // of its own, as the daemon runs it after answering.
    let (r, f) = call(&d, json!({"op": "hook", "event": "UserPromptSubmit", "payload": {"session_id": "s", "prompt": "Where do we cache the exchange rates and for how long"}}));
    assert!(r.get("error").is_none(), "{r}");
    let bg = std::thread::spawn(move || f.map(|f| f()));
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    let stop = hook(&d, "Stop", json!({"session_id": "s", "stop_hook_active": false, "last_assistant_message": "Done."}));
    let took = t0.elapsed();
    assert!(took < Duration::from_millis(800), "the end of the turn waited {took:?} for the search");
    assert!(stop["output"].get("decision").is_none(), "the end of the turn blocked: {stop}");
    bg.join().unwrap();

    // The search finished after the turn: offered with the next prompt, once.
    let next = ctx(&hook(&d, "UserPromptSubmit", json!({"session_id": "s", "prompt": "ok"})));
    assert!(next.contains("finished after that turn ended") && next.contains("Why it matters"), "{next}");
    let again = ctx(&hook(&d, "UserPromptSubmit", json!({"session_id": "s", "prompt": "fine"})));
    assert!(!again.contains("finished after that turn ended"), "offered twice: {again}");
}

/// A search that finished before the end of the turn (a long turn with no
/// tool call after it) does not block either: it too waits for the next
/// prompt. (Before CTX-1 this blocked 4 of 4 turns of 20 s.)
#[test]
fn a_finished_search_does_not_make_claude_answer_again() {
    home();
    let t = tempfile::tempdir().unwrap();
    let exe = fake_ranker(t.path(), 0.0);
    let store = tempfile::tempdir().unwrap();
    let d = daemon(store.path(), &exe, json!({"mode": "async", "planner": "none", "min_records": 1, "min_words": 2}));
    hook(&d, "SessionStart", json!({"session_id": "s", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s", "prompt": "Where do we cache the exchange rates and for how long"}));
    let stop = hook(&d, "Stop", json!({"session_id": "s", "stop_hook_active": false, "last_assistant_message": "Done."}));
    assert!(stop["output"].get("decision").is_none(), "{stop}");
    let next = ctx(&hook(&d, "UserPromptSubmit", json!({"session_id": "s", "prompt": "ok"})));
    assert!(next.contains("finished after that turn ended"), "the search's records were lost: {next}");
}

/// Turned on, the recheck waits at most a few seconds whatever
/// `stop_wait_secs` says, and its reason never asks for the answer again.
#[test]
fn the_opt_in_recheck_is_bounded_and_does_not_ask_for_a_repeat() {
    home();
    let t = tempfile::tempdir().unwrap();
    let exe = fake_ranker(t.path(), 0.0);
    let store = tempfile::tempdir().unwrap();
    let d = daemon(store.path(), &exe, json!({"mode": "async", "planner": "none", "min_records": 1, "min_words": 2, "stop_recheck": true, "stop_wait_secs": 25}));
    hook(&d, "SessionStart", json!({"session_id": "s", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s", "prompt": "Where do we cache the exchange rates and for how long"}));
    let stop = hook(&d, "Stop", json!({"session_id": "s", "stop_hook_active": false, "last_assistant_message": "Done."}));
    assert_eq!(stop["output"]["decision"], json!("block"), "{stop}\n{}", std::fs::read_to_string(store.path().join("events.log")).unwrap_or_default());
    let reason = stop["output"]["reason"].as_str().unwrap();
    assert!(!reason.contains("repeat your previous answer") && !reason.contains("complete answer again"), "{reason}");
    assert!(reason.contains("do not repeat your answer"), "{reason}");

    // A search that never finishes in time: the wait is capped.
    let slow = fake_ranker(t.path(), 30.0);
    let store = tempfile::tempdir().unwrap();
    let d = daemon(store.path(), &slow, json!({"mode": "async", "planner": "none", "min_records": 1, "min_words": 2, "stop_recheck": true, "stop_wait_secs": 25, "timeout_secs": 40}));
    hook(&d, "SessionStart", json!({"session_id": "s", "source": "startup"}));
    let (_, f) = call(&d, json!({"op": "hook", "event": "UserPromptSubmit", "payload": {"session_id": "s", "prompt": "Where do we cache the exchange rates and for how long"}}));
    std::thread::spawn(move || f.map(|f| f()));
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    let stop = hook(&d, "Stop", json!({"session_id": "s", "stop_hook_active": false, "last_assistant_message": "Done."}));
    let took = t0.elapsed();
    assert!(took < Duration::from_secs(chloectx::deep::STOP_WAIT_MAX_SECS + 1), "waited {took:?}");
    assert!(stop["output"].get("decision").is_none(), "{stop}");
}

/// The defaults: no recheck, no wait.
#[test]
fn defaults_never_hold_the_end_of_a_turn() {
    let c = chloectx::deep::DeepConfig::default();
    assert_eq!(c.mode, "async");
    assert!(!c.stop_recheck);
    assert_eq!(c.stop_wait(), Duration::ZERO);
    let on = chloectx::deep::DeepConfig { stop_recheck: true, stop_wait_secs: 25, ..Default::default() };
    assert_eq!(on.stop_wait(), Duration::from_secs(chloectx::deep::STOP_WAIT_MAX_SECS));
}
