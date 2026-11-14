//! `chloectx doctor` says when query understanding runs (after the prompt,
//! at prompt time, only when asked, or never), whether the end of a turn
//! waits for it, what it and the distiller cost in this store (from its
//! events.log), and whether the `claude` CLI they need is logged in. The
//! real program, in a home of the test's own, with a stand-in `claude`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use serde_json::json;

struct Home {
    t: tempfile::TempDir,
}

impl Home {
    fn new(logged_in: bool) -> Home {
        let t = tempfile::tempdir().unwrap();
        for d in ["home/.claude", "ctx", "proj", "bin"] {
            std::fs::create_dir_all(t.path().join(d)).unwrap();
        }
        let claude = t.path().join("bin/claude");
        std::fs::write(&claude, format!("#!/bin/sh\ncase \"$1\" in\n  --version) echo '2.1.0 (Claude Code)' ;;\n  auth) echo '{{\"loggedIn\": {logged_in}}}' ;;\nesac\n")).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        Home { t }
    }

    fn path(&self, p: &str) -> std::path::PathBuf {
        self.t.path().join(p)
    }

    /// The project's store folder, with these events in its events.log.
    fn events(&self, lines: &[serde_json::Value]) {
        let store = self.path("ctx/projects").join(chloectx::paths::project_slug(&self.path("proj").canonicalize().unwrap()));
        std::fs::create_dir_all(&store).unwrap();
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(store.join("events.log"), text).unwrap();
    }

    fn config(&self, cfg: serde_json::Value) {
        let store = self.path("ctx/projects").join(chloectx::paths::project_slug(&self.path("proj").canonicalize().unwrap()));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("config.json"), cfg.to_string()).unwrap();
    }

    fn user_settings(&self, v: serde_json::Value) {
        std::fs::write(self.path("home/.claude/settings.json"), v.to_string()).unwrap();
    }

    fn doctor(&self) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_chloectx"))
            .arg("doctor")
            .current_dir(self.path("proj"))
            .env("HOME", self.path("home"))
            .env("CHLOECTX_HOME", self.path("ctx"))
            .env("CHLOECTX_CLAUDE", self.path("bin/claude"))
            .env("CHLOECTX_EMBEDDER", "hash:64")
            .env("CHLOECTX_SOURCE", "/nonexistent")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CHLOECTX_DEEP")
            .env_remove("CHLOECTX_DISTILL")
            .env_remove("CHLOECTX_DISTILL_MODEL")
            .env_remove("CHLOECTX_PLUGIN")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

fn line<'a>(out: &'a str, what: &str) -> &'a str {
    out.lines().find(|l| l.contains(&format!(" {what}: "))).unwrap_or_else(|| panic!("no `{what}` line in:\n{out}"))
}

#[test]
fn doctor_says_what_query_understanding_and_the_distiller_do_and_cost() {
    let h = Home::new(true);
    let now = chloectx::now();
    h.events(&[
        json!({"ts": now - 40 * 86_400, "event": "deep", "ms": 99_000, "cost_usd": 1.0}),
        json!({"ts": now - 600, "event": "deep", "mode": "async", "ms": 7_000, "cost_usd": 0.0010}),
        json!({"ts": now - 500, "event": "deep", "mode": "async", "ms": 9_000, "cost_usd": 0.0012}),
        json!({"ts": now - 400, "event": "deep", "mode": "async", "ms": 60_000, "error": "claude: timed out"}),
        json!({"ts": now - 300, "event": "distill", "ms": 4_000, "cost_usd": 0.0008, "kept": 1}),
        json!({"ts": now - 200, "event": "recall"}),
    ]);
    let out = h.doctor();

    let qu = line(&out, "query understanding");
    assert!(qu.trim_start().starts_with("ok"), "{qu}");
    assert!(qu.contains("after the prompt, in the background (the default)") && qu.contains("2 model calls (haiku") && qu.contains("the end of a turn does not wait"), "{qu}");
    // Thirty days only: the old one is left out; the failed one is counted.
    let cost = line(&out, "query understanding cost");
    assert!(cost.contains("3 searches in the last 30 days, median 9.0 s, $0.002 in all ($0.0011 a search); 1 failed"), "{cost}");
    let dcost = line(&out, "distillation cost");
    assert!(dcost.contains("1 turn in the last 30 days, median 4.0 s, $0.001 in all ($0.0008 a turn)"), "{dcost}");
    let login = line(&out, "Claude Code login");
    assert!(login.contains("logged in, so the distiller and query understanding can run"), "{login}");
}

#[test]
fn doctor_warns_when_a_prompt_or_the_end_of_a_turn_waits() {
    let h = Home::new(true);
    // At prompt time, from the user settings Claude Code gives the hooks.
    h.user_settings(json!({"env": {"CHLOECTX_DEEP": "sync"}}));
    let out = h.doctor();
    let qu = line(&out, "query understanding");
    assert!(qu.trim_start().starts_with("warn") && qu.contains("at prompt time (CHLOECTX_DEEP in the user settings)") && qu.contains("the prompt waits for 2 model calls"), "{qu}");
    assert!(line(&out, "query understanding cost").contains("nothing measured"), "{out}");

    // After the prompt, with the end of a turn waiting for it.
    h.user_settings(json!({}));
    h.config(json!({"deep": {"stop_recheck": true, "stop_wait_secs": 9}}));
    let qu = line(&h.doctor(), "query understanding").to_string();
    assert!(qu.trim_start().starts_with("warn") && qu.contains("waits up to 3 s") && qu.contains("runs the main model again"), "{qu}");

    // Off: no model calls, and no cost line for it.
    h.config(json!({"deep": {"mode": "off"}, "distill": {"enabled": false}}));
    let out = h.doctor();
    assert!(line(&out, "query understanding").contains("off (the store's config.json): no model calls"), "{out}");
    assert!(!out.contains("query understanding cost") && !out.contains("distillation cost"), "{out}");
    assert!(!out.contains("Claude Code login"), "nothing needs the CLI's login: {out}");
}

#[test]
fn doctor_fails_when_the_cli_the_model_calls_need_is_logged_out() {
    let h = Home::new(false);
    let out = h.doctor();
    let login = line(&out, "Claude Code login");
    assert!(login.trim_start().starts_with("FAIL") && login.contains("so the distiller and query understanding cannot run") && login.contains("claude auth login"), "{login}");
    // Only query understanding needs it, with the distiller off.
    let h = Home::new(false);
    h.user_settings(json!({"env": {"CHLOECTX_DISTILL": "0"}}));
    let login = line(&h.doctor(), "Claude Code login").to_string();
    assert!(login.contains("so query understanding cannot run"), "{login}");
    assert!(Path::new(env!("CARGO_BIN_EXE_chloectx")).exists());
}
