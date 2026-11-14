//! Receipts, kept snapshots and blame over a daemon's lifecycle: a decision
//! recalled before an edit, the edit blamed back to it, the recall replayed
//! on the snapshot kept at the time (identical) and on today's store (the
//! decision since superseded). Uses the hash embedder, so no model files
//! are needed.

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

fn ok(r: Json) -> Json {
    assert!(r.get("error").is_none(), "{r}");
    r
}

#[test]
fn a_line_is_blamed_on_the_recall_before_its_edit_and_the_recall_replays() {
    let store = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let project = proj.path().canonicalize().unwrap().to_string_lossy().into_owned();
    std::fs::create_dir_all(proj.path().join("src")).unwrap();
    std::fs::write(proj.path().join("src/retry.rs"), "fn retry() {\n    sleep(BACKOFF_FIXED_2S);\n}\n").unwrap();
    std::fs::write(store.path().join("config.json"), r#"{"embedder": "hash:64", "sync": false, "distill": {"enabled": false}}"#).unwrap();
    let d = Arc::new(Daemon::open(store.path(), &project).unwrap());

    // Session one agrees on a backoff policy.
    hook(&d, "SessionStart", json!({"session_id": "s1", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "How should payment retries back off?"}));
    let r = ok(call(&d, json!({"op": "remember", "kind": "decision", "text": "Payment retries back off with a fixed 2 second delay between attempts."})));
    let decision = r["id"].as_str().unwrap().to_string();
    hook(&d, "Stop", json!({"session_id": "s1", "last_assistant_message": "Agreed: payment retries use a fixed 2 second backoff."}));

    // Session two: the prompt recalls the decision, then the file is edited.
    let brief = hook(&d, "SessionStart", json!({"session_id": "s2", "source": "startup"}));
    assert!(brief.contains("fixed 2 second delay"), "the decision is not in the briefing: {brief}");
    let ctx = hook(&d, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "Change this: payment retries back off with a fixed 2 second delay between attempts."}));
    assert!(!ctx.contains("fixed 2 second delay"), "the briefing's decision was shown again: {ctx}");
    hook(&d, "PostToolUse", json!({"session_id": "s2", "tool_name": "Read", "tool_input": {"file_path": format!("{project}/src/retry.rs")}, "tool_response": {"content": "fn retry() {}"}}));
    hook(&d, "PostToolUse", json!({"session_id": "s2", "tool_name": "Edit",
        "tool_input": {"file_path": format!("{project}/src/retry.rs"), "old_string": "sleep(BACKOFF_FIXED_2S);", "new_string": "sleep(backoff_delay(attempt));"}, "tool_response": {}}));
    std::fs::write(proj.path().join("src/retry.rs"), "fn retry() {\n    sleep(backoff_delay(attempt));\n}\n").unwrap();
    hook(&d, "Stop", json!({"session_id": "s2", "last_assistant_message": "The retry now sleeps backoff_delay(attempt)."}));

    // The briefing's receipt names the decision; a snapshot was kept at
    // the commit it read, and the replay there is identical. The prompt's
    // receipt says the decision was left out as shown before.
    let rs = ok(call(&d, json!({"op": "receipts", "session": "s2"})));
    let list = rs["receipts"].as_array().unwrap();
    let brc = list.iter().find(|r| r["source"] == "briefing").unwrap_or_else(|| panic!("{rs}"));
    assert!(brc["shown"].as_array().unwrap().iter().any(|i| i == decision.as_str()), "{brc}");
    // With the rows it showed and their text (`chloectx receipts --json`).
    let shown = brc["rows"].as_array().unwrap().iter().find(|r| r["id"] == decision.as_str()).unwrap_or_else(|| panic!("{brc}"));
    assert_eq!((shown["kind"].as_str(), shown["decision"].as_str()), (Some("decision"), Some("briefing")), "{shown}");
    assert!(shown["text"].as_str().unwrap().contains("fixed 2 second delay"), "{shown}");
    assert!(shown["score"].is_number(), "{shown}");
    let id = brc["id"].as_str().unwrap().to_string();
    let full = ok(call(&d, json!({"op": "receipt", "id": id, "text": true})));
    let receipt = &full["receipt"];
    assert_eq!(receipt["v"], json!(1));
    assert!(receipt["text"].as_str().unwrap().contains("fixed 2 second delay"), "{full}");
    let row = receipt["rows"].as_array().unwrap().iter().find(|r| r["id"] == decision.as_str()).unwrap();
    assert_eq!(row["decision"], json!("briefing"), "{row}");
    assert!(row["tokens"].as_u64().unwrap() > 0 && row["sha"].as_str().unwrap().len() == 16, "{row}");
    let replays = full["replays"].as_array().unwrap();
    assert_eq!(replays[0]["on"], json!("kept snapshot"), "{full}");
    assert_eq!(replays[0]["verdict"]["identical"], json!(true), "{}", full["text"]);
    let prc = list.iter().find(|r| r["source"] == "prompt" && r["turn"] == 1).unwrap_or_else(|| panic!("{rs}"));
    let p = ok(call(&d, json!({"op": "receipt", "id": prc["id"]})));
    assert!(!p["receipt"]["shown"].as_array().unwrap().is_empty(), "{p}");
    assert!(p["receipt"]["rows"].as_array().unwrap().iter().all(|r| r["id"] != decision.as_str() || r["decision"] == "skip: shown before"), "{p}");
    assert_eq!(p["replays"][0]["on"], json!("kept snapshot"), "{p}");
    assert_eq!(p["replays"][0]["verdict"]["identical"], json!(true), "{}", p["text"]);

    // The decision is superseded.
    ok(call(&d, json!({"op": "remember", "kind": "decision", "text": "Payment retries back off exponentially, capped at 5 seconds.", "supersedes": decision})));

    // Blame the line: the edit, its prompt, the recalled decision flagged
    // as superseded since, and the replay: identical then, different now.
    let b = ok(call(&d, json!({"op": "blame", "target": "src/retry.rs:2"})));
    let text = b["text"].as_str().unwrap();
    assert!(b["how"].as_str().unwrap().contains("added the line"), "{text}");
    assert_eq!(b["edit"]["session"], json!("s2"), "{text}");
    assert!(text.contains("Change this: payment retries"), "{text}");
    assert!(text.contains("Read src/retry.rs"), "{text}");
    assert!(text.contains(&decision) && text.contains("superseded since"), "{text}");
    let replays = b["replays"].as_array().unwrap();
    assert_eq!(replays[0]["verdict"]["identical"], json!(true), "{text}");
    assert!(text.contains("worth a look"), "{text}");
    // Today the briefing no longer holds the superseded decision.
    let now = ok(call(&d, json!({"op": "receipt", "id": id})));
    let v = &now["replays"].as_array().unwrap().iter().find(|r| r["on"] == "now").unwrap()["verdict"];
    assert_eq!(v["same_shown"], json!(false), "{}", now["text"]);

    // A file nobody edited in a session has nothing to blame.
    std::fs::write(proj.path().join("README"), "hello\n").unwrap();
    let b = ok(call(&d, json!({"op": "blame", "target": "README:1"})));
    assert!(b["edit"].is_null(), "{b}");

    // A named snapshot is kept until dropped; names are checked.
    let k = ok(call(&d, json!({"op": "snapshot", "what": "keep", "name": "before-release"})));
    assert!(k["text"].as_str().unwrap().starts_with("kept before-release"), "{k}");
    assert!(call(&d, json!({"op": "snapshot", "what": "keep", "name": "../x"})).get("error").is_some());
    let l = ok(call(&d, json!({"op": "snapshot", "what": "list"})));
    assert!(l["snapshots"].as_array().unwrap().iter().any(|k| k["name"] == "before-release" && k["auto"] == false), "{l}");

    // A search answer carries its receipt.
    let s = ok(call(&d, json!({"op": "search", "query": "payment retries backoff", "include_current": true})));
    assert!(s["text"].as_str().unwrap().contains("(receipt rc-"), "{s}");

    // Forgetting the decision scrubs the receipts that showed it and drops
    // the automatic snapshots, which hold its text.
    let f = ok(call(&d, json!({"op": "forget", "id": decision})));
    let ft = f["text"].as_str().unwrap();
    assert!(ft.contains("scrubbed") && ft.contains("automatic snapshot") && ft.contains("before-release"), "{ft}");
    let after = ok(call(&d, json!({"op": "receipt", "id": id, "replay": false})));
    assert!(!after["receipt"]["text"].as_str().unwrap().contains("fixed 2 second"), "{after}");
    let l = ok(call(&d, json!({"op": "snapshot", "what": "list"})));
    assert!(l["snapshots"].as_array().unwrap().iter().all(|k| k["auto"] == false), "{l}");
    ok(call(&d, json!({"op": "snapshot", "what": "drop", "name": "before-release"})));

    // Receipts survive a restart.
    drop(d);
    let d = Arc::new(Daemon::open(store.path(), &project).unwrap());
    assert!(ok(call(&d, json!({"op": "receipt", "id": id, "replay": false})))["receipt"]["id"] == json!(id));
}

#[test]
fn receipts_can_be_turned_off() {
    let store = tempfile::tempdir().unwrap();
    std::fs::write(store.path().join("config.json"), r#"{"embedder": "hash:64", "sync": false, "distill": {"enabled": false}, "receipts": {"enabled": false}}"#).unwrap();
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "anything"}));
    assert!(!store.path().join("receipts.strata").exists());
    assert!(call(&d, json!({"op": "receipts"})).get("error").is_some());
}

/// CTX-6: a record a search listed before an edit (a `listed` row, not one
/// shown in full) and still in the store is not called "deleted since".
#[test]
fn blame_does_not_call_listed_records_deleted() {
    let store = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let project = proj.path().canonicalize().unwrap().to_string_lossy().into_owned();
    std::fs::create_dir_all(proj.path().join("src")).unwrap();
    std::fs::write(proj.path().join("src/tax.rs"), "fn tax() {\n    round_half_even(x)\n}\n").unwrap();
    std::fs::write(store.path().join("config.json"), r#"{"embedder": "hash:64", "sync": false, "distill": {"enabled": false}}"#).unwrap();
    let d = Arc::new(Daemon::open(store.path(), &project).unwrap());
    hook(&d, "SessionStart", json!({"session_id": "s1", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "How do we round tax amounts?"}));
    for t in ["Tax amounts round half to even at the last step.", "The tax test suite runs with cargo test tax.", "Tax rounding is checked against the VAT tables."] {
        ok(call(&d, json!({"op": "remember", "kind": "note", "text": t})));
    }
    hook(&d, "Stop", json!({"session_id": "s1", "last_assistant_message": "Tax rounds half to even."}));

    hook(&d, "SessionStart", json!({"session_id": "s2", "source": "startup"}));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "Change the tax rounding"}));
    // A search lists records (the `listed` rows of its receipt).
    let s = ok(call(&d, json!({"op": "search", "query": "tax rounding", "include_current": true})));
    let listed: Vec<String> = s["ids"].as_array().unwrap().iter().filter_map(|v| v.as_str().map(String::from)).collect();
    assert!(!listed.is_empty(), "{s}");
    hook(&d, "PostToolUse", json!({"session_id": "s2", "tool_name": "Edit",
        "tool_input": {"file_path": format!("{project}/src/tax.rs"), "old_string": "round_half_even(x)", "new_string": "round_half_up(x)"}, "tool_response": {}}));
    std::fs::write(proj.path().join("src/tax.rs"), "fn tax() {\n    round_half_up(x)\n}\n").unwrap();
    hook(&d, "Stop", json!({"session_id": "s2", "last_assistant_message": "Changed."}));

    let b = ok(call(&d, json!({"op": "blame", "target": "src/tax.rs:2"})));
    let text = b["text"].as_str().unwrap();
    assert_eq!(b["edit"]["session"], json!("s2"), "{text}");
    assert!(listed.iter().any(|id| text.contains(id.as_str())), "the search's rows are not in the blame: {text}");
    assert!(!text.contains("deleted since"), "records that exist are called deleted: {text}");
}
