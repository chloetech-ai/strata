//! Memory hygiene around receipts, kept snapshots and forget (the second
//! audit's N-F2..N-F13, R2..R6): what a forget leaves, what retention
//! removes and when, the receipts' off switch, and blame's targets. Uses
//! the hash embedder, so no model files are needed.

use std::path::Path;
use std::sync::Arc;

use chloectx::daemon::Daemon;
use chloectx::receipt::{Book, BookRetention, Forgotten, Receipt};
use serde_json::{json, Value as Json};

fn call(d: &Arc<Daemon>, req: Json) -> Json {
    let (resp, deferred) = d.handle(&req);
    if let Some(f) = deferred {
        f();
    }
    resp
}

fn ok(r: Json) -> Json {
    assert!(r.get("error").is_none(), "{r}");
    r
}

fn hook(d: &Arc<Daemon>, event: &str, payload: Json) -> String {
    let r = ok(call(d, json!({"op": "hook", "event": event, "payload": payload})));
    r["output"]["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
}

fn config(dir: &Path, extra: &str) {
    std::fs::write(dir.join("config.json"), format!(r#"{{"embedder": "hash:64", "sync": false, "distill": {{"enabled": false}}{extra}}}"#)).unwrap();
}

/// Every file under `dir` whose bytes hold `s`.
fn files_holding(dir: &Path, s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_holding(&p, s));
        } else if std::fs::read(&p).is_ok_and(|b| b.windows(s.len()).any(|w| w == s.as_bytes())) {
            out.push(p.display().to_string());
        }
    }
    out
}

fn snapshot_names(d: &Arc<Daemon>) -> Vec<(String, bool)> {
    let l = ok(call(d, json!({"op": "snapshot", "what": "list"})));
    l["snapshots"].as_array().unwrap().iter().map(|k| (k["name"].as_str().unwrap().to_string(), k["auto"] == true)).collect()
}

#[test]
fn a_forgotten_record_is_in_no_snapshot_taken_after_the_forget() {
    // N-F2: the next snapshot used to copy the segment the record was still in.
    let store = tempfile::tempdir().unwrap();
    config(store.path(), r#", "receipts": {"snapshots": {"min_interval_secs": 0}}"#);
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    let secret = ok(call(&d, json!({"op": "remember", "kind": "fact", "text": "The staging database password is quokka-zebra-7731 for the payments cluster."})))["id"].as_str().unwrap().to_string();
    ok(call(&d, json!({"op": "remember", "kind": "decision", "text": "Payments retries back off exponentially, capped at five seconds."})));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "What is the staging database password for the payments cluster?"}));
    assert!(!files_holding(&store.path().join("snapshots"), "quokka-zebra-7731").is_empty(), "the first snapshot holds the record");

    let f = ok(call(&d, json!({"op": "forget", "id": secret})));
    assert!(f["text"].as_str().unwrap().contains("automatic snapshot"), "{f}");
    // A later turn that shows records keeps a new snapshot.
    hook(&d, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "How do payments retries back off?"}));
    assert!(snapshot_names(&d).iter().any(|(_, auto)| *auto), "a snapshot was kept after the forget");
    let left = files_holding(&store.path().join("snapshots"), "quokka-zebra-7731");
    assert!(left.is_empty(), "the forgotten text is in {left:?}");
}

#[test]
fn automatic_snapshots_are_kept_at_most_once_an_hour_by_default() {
    // N-F7: a snapshot flushes the store; one per turn more than doubled
    // the store's write amplification.
    let store = tempfile::tempdir().unwrap();
    config(store.path(), "");
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    ok(call(&d, json!({"op": "remember", "kind": "decision", "text": "Payments retries back off exponentially, capped at five seconds."})));
    for (i, p) in ["How do payments retries back off?", "Remind me: payments retries back off how?", "payments retries back off, capped?"].iter().enumerate() {
        hook(&d, "UserPromptSubmit", json!({"session_id": format!("s{i}"), "prompt": p}));
        ok(call(&d, json!({"op": "remember", "kind": "note", "text": format!("note {i} about payments")})));
    }
    let autos = snapshot_names(&d).into_iter().filter(|(_, a)| *a).count();
    assert_eq!(autos, 1, "{:?}", snapshot_names(&d));
    // Each prompt's own record is stored all the same.
    let rs = ok(call(&d, json!({"op": "sql", "sql": "SELECT COUNT(*) FROM memory WHERE kind = 'prompt'"})));
    assert!(rs["text"].as_str().unwrap().contains('3'), "{rs}");
}

#[test]
fn retention_runs_when_the_daemon_starts_even_with_receipts_off() {
    // N-F4: pruning ran only after a new automatic snapshot was taken.
    // N-F12: what no snapshot owns under snapshots/ is removed.
    let store = tempfile::tempdir().unwrap();
    config(store.path(), r#", "receipts": {"snapshots": {"min_interval_secs": 0}}"#);
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    ok(call(&d, json!({"op": "remember", "kind": "decision", "text": "Payments retries back off exponentially, capped at five seconds."})));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "How do payments retries back off?"}));
    ok(call(&d, json!({"op": "snapshot", "what": "keep", "name": "release"})));
    assert_eq!(snapshot_names(&d).len(), 2);
    drop(d);

    // The automatic one is 40 days old; a stopped daemon left a checkpoint
    // half written, and a crash left a directory no entry names.
    let snaps = store.path().join("snapshots");
    let idx = snaps.join("index.json");
    let mut v: Vec<Json> = serde_json::from_str(&std::fs::read_to_string(&idx).unwrap()).unwrap();
    for k in v.iter_mut().filter(|k| k["auto"] == true) {
        k["ts"] = json!(chloectx::now() - 40 * 86_400);
    }
    std::fs::write(&idx, serde_json::to_string(&v).unwrap()).unwrap();
    std::fs::create_dir_all(snaps.join(".auto-99.checkpoint-x1y2z3")).unwrap();
    std::fs::write(snaps.join(".auto-99.checkpoint-x1y2z3/seg"), "bytes").unwrap();
    std::fs::create_dir_all(snaps.join("auto-7")).unwrap();

    config(store.path(), r#", "receipts": {"enabled": false}"#);
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    d.housekeep();
    assert_eq!(snapshot_names(&d), vec![("release".to_string(), false)]);
    let mut left: Vec<String> = std::fs::read_dir(&snaps).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, vec!["index.json", "release"]);
}

#[test]
fn a_snapshot_is_kept_when_the_one_it_would_link_from_does_not_read() {
    // N-F12: a newest entry whose directory a crash half removed stopped
    // every later snapshot.
    let store = tempfile::tempdir().unwrap();
    config(store.path(), r#", "receipts": {"snapshots": {"min_interval_secs": 0}}"#);
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    ok(call(&d, json!({"op": "remember", "kind": "decision", "text": "Payments retries back off exponentially."})));
    ok(call(&d, json!({"op": "snapshot", "what": "keep", "name": "a"})));
    for e in std::fs::read_dir(store.path().join("snapshots/a")).unwrap().flatten() {
        let _ = std::fs::remove_file(e.path());
    }
    ok(call(&d, json!({"op": "remember", "kind": "note", "text": "another"})));
    let k = ok(call(&d, json!({"op": "snapshot", "what": "keep", "name": "b"})));
    assert!(k["text"].as_str().unwrap().starts_with("kept b"), "{k}");
}

#[test]
fn receipts_off_still_lets_forget_scrub_the_old_ones_and_the_switch_holds() {
    // N-F6.
    let store = tempfile::tempdir().unwrap();
    config(store.path(), "");
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    let id = ok(call(&d, json!({"op": "remember", "kind": "fact", "text": "The vendor's API key prefix for payments is vndr-live."})))["id"].as_str().unwrap().to_string();
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "what is the vendor API key prefix for payments?"}));
    let before = ok(call(&d, json!({"op": "receipts"})))["receipts"].as_array().unwrap().len();
    assert!(before >= 1);
    drop(d);

    config(store.path(), r#", "receipts": {"enabled": false}"#);
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    let f = ok(call(&d, json!({"op": "forget", "id": id})));
    assert!(f["text"].as_str().unwrap().contains("receipt"), "the old receipts are scrubbed with receipts off: {f}");
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "anything about payments?"}));
    assert_eq!(ok(call(&d, json!({"op": "receipts"})))["receipts"].as_array().unwrap().len(), before, "none written while off");
    drop(d);

    // The environment of the hook turns receipts off when config.json is silent.
    config(store.path(), "");
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    ok(call(&d, json!({"op": "hook", "event": "UserPromptSubmit", "receipts": false, "payload": {"session_id": "s2", "prompt": "payments vendor?"}})));
    assert_eq!(ok(call(&d, json!({"op": "receipts"})))["receipts"].as_array().unwrap().len(), before);

    // `chloectx receipts off --purge`: in config.json, which wins from then on.
    let r = ok(call(&d, json!({"op": "receipts_switch", "on": false, "purge": true})));
    assert!(r["text"].as_str().unwrap().contains("off"), "{r}");
    let cfg: Json = serde_json::from_str(&std::fs::read_to_string(store.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(cfg["receipts"]["enabled"], json!(false));
    assert_eq!(cfg["embedder"], json!("hash:64"), "the rest of config.json is kept");
    assert_eq!(ok(call(&d, json!({"op": "receipts"})))["receipts"].as_array().unwrap().len(), 0);
    ok(call(&d, json!({"op": "hook", "event": "UserPromptSubmit", "receipts": true, "payload": {"session_id": "s2", "prompt": "payments vendor?"}})));
    assert_eq!(ok(call(&d, json!({"op": "receipts"})))["receipts"].as_array().unwrap().len(), 0, "the switch wins over the hook's environment");
    ok(call(&d, json!({"op": "receipts_switch", "on": true})));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s2", "prompt": "payments vendor?"}));
    assert_eq!(ok(call(&d, json!({"op": "receipts"})))["receipts"].as_array().unwrap().len(), 1);

    // N-F9: sizes in stats.
    let s = ok(call(&d, json!({"op": "stats"})));
    assert_eq!(s["receipts"]["on"], json!(true), "{s}");
    assert!(s["receipts"]["count"].as_u64().unwrap() >= 1 && s["snapshots"]["bytes"].is_u64() && s["total_bytes"].as_u64().unwrap() > 0, "{s}");
}

#[test]
fn forgetting_a_prompt_removes_it_from_its_receipts_query() {
    // R2.
    let store = tempfile::tempdir().unwrap();
    config(store.path(), "");
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    ok(call(&d, json!({"op": "remember", "kind": "decision", "text": "Payments retries back off exponentially."})));
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "payments retries: my card is 4111 2222 3333 4444 hmm"}));
    let rs = ok(call(&d, json!({"op": "sql", "sql": "SELECT id FROM memory WHERE kind = 'prompt' LIMIT 1"})));
    let pid = rs["text"].as_str().unwrap().lines().map(str::trim).find(|l| l.starts_with("s1:")).unwrap_or_else(|| panic!("{rs}")).to_string();
    let list = ok(call(&d, json!({"op": "receipts", "session": "s1"})));
    let rid = list["receipts"][0]["id"].as_str().unwrap().to_string();
    let before = ok(call(&d, json!({"op": "receipt", "id": rid, "replay": false})));
    assert!(before["receipt"]["inputs"]["query"].as_str().unwrap().contains("payments retries"), "{before}");
    ok(call(&d, json!({"op": "forget", "id": pid})));
    let after = ok(call(&d, json!({"op": "receipt", "id": rid, "replay": false})));
    let q = after["receipt"]["inputs"]["query"].as_str().unwrap();
    assert!(!q.contains("payments retries") && q.contains("forgotten"), "{q}");
    // R4: a receipt names the version that wrote it.
    assert_eq!(after["receipt"]["chloectx"], json!(env!("CARGO_PKG_VERSION")));
}

fn receipt(id: &str, session: &str, turn: i64, ts: i64, shown: &[&str]) -> Receipt {
    serde_json::from_value(json!({
        "v": 1, "id": id, "ts": ts, "session": session, "turn": turn, "source": "prompt",
        "snapshot": {"txn": 1, "seq_no": null, "last_seq": 0, "records": 0}, "embedder": "hash:64",
        "inputs": serde_json::to_value(chloectx::receipt::Inputs::search("q", &Default::default(), 0, &Default::default())).unwrap(),
        "decision": null, "gate": 0.0, "evidence": serde_json::to_value(chloectx::recall::Evidence::default()).unwrap(), "candidates": 0,
        "rows": shown.iter().map(|s| json!({"rank": 1, "id": s, "kind": "fact", "tag": "-", "score": 1.0, "bm25": null, "cos": null, "cov": null,
            "importance": 0.5, "age_seq": 0, "tokens": 3, "decision": "take", "sha": "0123456789abcdef"})).collect::<Vec<_>>(),
        "shown": shown, "text": format!("[{}] the text", shown.first().unwrap_or(&"")), "text_sha": "", "tokens": 3,
    }))
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn the_book_flushes_pages_its_scrub_prunes_and_reads_late_turns() {
    let dir = tempfile::tempdir().unwrap();
    let book = Book::open(dir.path(), false).unwrap();
    let now = chloectx::now();
    // 1,200 receipts, three of which showed s1:9, one per page and beyond.
    let mut all = Vec::new();
    for i in 0..1200i64 {
        let shown: Vec<&str> = if [10, 700, 1150].contains(&i) { vec!["s1:9"] } else { vec!["s1:1"] };
        all.push(receipt(&format!("rc-{i:05}"), "s1", i, now - 100 * 86_400 + i * 86_400 / 10, &shown));
    }
    for chunk in all.chunks(100) {
        book.put_all(chunk).unwrap();
    }
    // N-F3: the log is flushed into segments as it grows.
    let (_, wal) = book.bytes();
    assert!(wal < 1200 * 600, "the WAL holds every receipt: {wal} bytes");

    // R6: scrubbed a page at a time; R2: the rows lose the text's hash.
    let n = book.scrub(Some(&Forgotten { id: "s1:9".into(), prompt: None })).unwrap();
    assert_eq!(n, 3);
    let r = book.get("rc-00700").unwrap().unwrap();
    assert!(r.text.contains("forgotten") && r.rows[0].sha.is_empty(), "{:?}", r.text);
    assert!(!book.get("rc-00701").unwrap().unwrap().rows[0].sha.is_empty());

    // N-F11: a late turn's receipts are read, not the session's oldest 500.
    let late = book.of_turns("s1", 1190, 1199).unwrap();
    assert_eq!(late.len(), 10);
    assert!(late.windows(2).all(|w| w[0].ts <= w[1].ts), "oldest first");

    // Retention: older than 90 days, then at most max_receipts.
    let dropped = book.prune(&BookRetention::default()).unwrap();
    assert!(dropped > 0 && book.len() < 1200, "{dropped}");
    assert!(book.get("rc-00000").unwrap().is_none());
    book.prune(&BookRetention { keep_days: 365, max_receipts: 50 }).unwrap();
    assert_eq!(book.len(), 50);
    assert!(book.get("rc-01199").unwrap().is_some(), "the newest are kept");
    book.flush().unwrap();
    drop(book);
    let book = Book::open(dir.path(), false).unwrap();
    assert_eq!(book.len(), 50);
}

#[test]
fn blame_resolves_a_relative_target_against_the_project_and_wants_an_added_line() {
    // N-F13: the daemon's working directory is not the project. R5: a line the edit only
    // kept is not one it wrote.
    let store = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let project = proj.path().canonicalize().unwrap().to_string_lossy().into_owned();
    std::fs::create_dir_all(proj.path().join("src")).unwrap();
    std::fs::write(proj.path().join("src/lib.rs"), "fn keep() {}\nfn added() {}\n").unwrap();
    // The daemon was started by a hook run in a subdirectory with a
    // src/lib.rs of its own.
    std::fs::create_dir_all(proj.path().join("web/src")).unwrap();
    std::fs::write(proj.path().join("web/src/lib.rs"), "export {}\nexport const web = 1;\n").unwrap();
    std::env::set_current_dir(proj.path().join("web")).unwrap();
    config(store.path(), "");
    let d = Arc::new(Daemon::open(store.path(), &project).unwrap());
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "add a function"}));
    hook(&d, "PostToolUse", json!({"session_id": "s1", "tool_name": "Edit",
        "tool_input": {"file_path": format!("{project}/src/lib.rs"), "old_string": "fn keep() {}", "new_string": "fn keep() {}\nfn added() {}"}, "tool_response": {}}));
    let b = ok(call(&d, json!({"op": "blame", "target": "src/lib.rs:2"})));
    assert!(b["text"].as_str().unwrap().contains("fn added()"), "the project's file was read: {b}");
    assert!(b["how"].as_str().unwrap().contains("added the line"), "{b}");
    let b = ok(call(&d, json!({"op": "blame", "target": "src/lib.rs:1"})));
    assert!(b["how"].as_str().unwrap().starts_with("no recorded edit"), "the edit kept line 1, it did not write it: {b}");
}

/// The ids of the store's on-disk segments.
fn segment_ids(d: &Arc<Daemon>) -> Vec<u64> {
    let mut v: Vec<u64> = chloectx::daemon::segments_live(d.store.db().unwrap()).into_keys().collect();
    v.sort();
    v
}

#[test]
fn a_forget_rewrites_only_the_segments_that_held_the_record_and_leaves_its_text_in_no_file() {
    // X-F2: the forget rewrote every segment with any deleted row, then
    // vacuumed the whole file, before it answered. N-F2 (a): the live
    // file's freed pages were never checked.
    let store = tempfile::tempdir().unwrap();
    config(store.path(), r#", "receipts": {"snapshots": {"min_interval_secs": 0}}"#);
    let d = Arc::new(Daemon::open(store.path(), "/proj").unwrap());
    let db = d.store.db().unwrap().clone();
    // An old segment with a deleted row of its own (a superseded note).
    let old = ok(call(&d, json!({"op": "remember", "kind": "note", "text": "Old note about the build cache, replaced later."})))["id"].as_str().unwrap().to_string();
    ok(call(&d, json!({"op": "remember", "kind": "decision", "text": "Payments retries back off exponentially, capped at five seconds."})));
    db.storage().flush().unwrap();
    ok(call(&d, json!({"op": "forget", "id": old})));
    let after_first = segment_ids(&d);
    // A second segment holds the secret.
    let secret = ok(call(&d, json!({"op": "remember", "kind": "fact", "text": "The staging database password is quokka-zebra-7731 for the payments cluster."})))["id"].as_str().unwrap().to_string();
    ok(call(&d, json!({"op": "remember", "kind": "note", "text": "The deploy window is Tuesday afternoons."})));
    db.storage().flush().unwrap();
    // A third, after it, with a deleted row too.
    let other = ok(call(&d, json!({"op": "remember", "kind": "note", "text": "Temporary note, deleted by hand."})))["id"].as_str().unwrap().to_string();
    ok(call(&d, json!({"op": "remember", "kind": "note", "text": "Lint runs before tests."})));
    db.storage().flush().unwrap();
    assert!(d.store.delete(&other).unwrap());
    hook(&d, "UserPromptSubmit", json!({"session_id": "s1", "prompt": "What is the staging database password for the payments cluster?"}));
    assert!(!files_holding(store.path(), "quokka-zebra-7731").is_empty());
    let before = segment_ids(&d);

    let f = ok(call(&d, json!({"op": "forget", "id": secret})));
    let text = f["text"].as_str().unwrap();
    assert!(text.contains("being rewritten"), "{text}");
    let now = segment_ids(&d);
    // The segments from before the secret's, and the one after it, are
    // the same segments: only the secret's was rewritten.
    for id in &after_first {
        assert!(now.contains(id), "segment {id} of the first flush was rewritten: {before:?} -> {now:?}");
    }
    assert_eq!(before.iter().filter(|id| !now.contains(id)).count(), 1, "{before:?} -> {now:?}");
    let left = files_holding(store.path(), "quokka-zebra-7731");
    assert!(left.is_empty(), "the forgotten text is in {left:?}");
}
