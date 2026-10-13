//! `engram hook <event>`: reads the hook payload on stdin, forwards it to the
//! project's daemon, and prints what the daemon wants injected. It never
//! fails the hook: on any error it prints nothing and exits 0.

use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::{daemon, paths};

/// How long each event may wait for the daemon (the first one may start it).
fn budget(event: &str) -> Duration {
    Duration::from_millis(match event {
        "SessionStart" => 8000,
        "UserPromptSubmit" => 3000,
        "PostCompact" => 5000,
        "PreToolUse" => 1500,
        _ => 2000,
    })
}

pub fn run(event: &str) -> i32 {
    if std::env::var("ENGRAM_DISABLE").map_or(false, |v| v == "1") {
        return 0;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let payload: Json = serde_json::from_str(&input).unwrap_or(Json::Null);
    let event = payload.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or(event).to_string();
    let project = paths::project_dir(payload.get("cwd").and_then(|v| v.as_str()));
    let dir = paths::store_dir(&project);
    let req = json!({"op": "hook", "event": event, "payload": payload});
    match daemon::request(&dir, &project.to_string_lossy(), &req, budget(&event)) {
        Ok(resp) => {
            if let Some(out) = resp.get("output").filter(|o| !o.is_null()) {
                println!("{out}");
            }
            if let Some(e) = resp.get("error") {
                eprintln!("engram: {e}");
            }
        }
        Err(e) => eprintln!("engram: {e}"),
    }
    0
}
