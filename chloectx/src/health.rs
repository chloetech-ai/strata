//! What the session should hear about ChloeCtx itself when it starts: the
//! failures `chloectx doctor` would report that the daemon can see on its
//! own (notes that stopped being written because distillation fails, most
//! often because the `claude` CLI's login expired), and a newer version in
//! the checkout the binary was built from.
//!
//! Without this a failing install looks like a working one: hooks record,
//! recall answers, and nothing new is ever written down.

use std::path::Path;

use serde_json::Value as Json;

/// The distillations that failed since the last one that worked, newest
/// first, from the tail of `events.log`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DistillStreak {
    /// Failed in a row, up to the newest.
    pub failed: usize,
    /// The newest failure's message.
    pub error: String,
    /// When the newest one failed.
    pub ts: i64,
}

/// The run of failed distillations at the end of the store's event log
/// (its last 256 KB).
pub fn distill_streak(store: &Path) -> DistillStreak {
    use std::io::{Read, Seek, SeekFrom};
    let mut out = DistillStreak::default();
    let Ok(mut f) = std::fs::File::open(store.join("events.log")) else { return out };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(256 * 1024))).is_err() {
        return out;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return out;
    }
    for e in buf.split(|&b| b == b'\n').rev().filter_map(|l| serde_json::from_slice::<Json>(l).ok()).filter(|e| e["event"] == "distill") {
        match e.get("error").and_then(|v| v.as_str()) {
            Some(err) => {
                if out.failed == 0 {
                    out.error = err.to_string();
                    out.ts = e["ts"].as_i64().unwrap_or(0);
                }
                out.failed += 1;
            }
            None => break,
        }
    }
    out
}

/// Whether a model call failed for want of a login (the `claude` CLI's
/// expired or missing, or a refused key).
pub fn is_login_error(error: &str) -> bool {
    let e = error.to_lowercase();
    ["/login", "log in", "logged out", "login", "oauth", "api key", "authenticat", "401", "unauthorized", "credit balance"].iter().any(|w| e.contains(w))
}

/// What to tell the person and the model at the start of a session.
#[derive(Debug, Default, PartialEq)]
pub struct Notice {
    /// Shown to the person (the hook's `systemMessage`).
    pub user: Vec<String>,
    /// Put at the top of the briefing, for the model.
    pub model: Vec<String>,
}

impl Notice {
    pub fn is_empty(&self) -> bool {
        self.user.is_empty() && self.model.is_empty()
    }
}

/// The notice for a session starting on the store in `dir`.
pub fn session_notice(dir: &Path, home: &Path) -> Notice {
    let mut n = Notice::default();
    let s = distill_streak(dir);
    if s.failed > 0 {
        let fix = if is_login_error(&s.error) {
            "the `claude` CLI is not logged in; run `claude auth login` in a terminal"
        } else {
            "run `chloectx doctor` to see why"
        };
        let what = if s.failed == 1 { "the last distillation failed".to_string() } else { format!("the last {} distillations failed", s.failed) };
        let err = crate::llm::clip(s.error.lines().next().unwrap_or(""), 160);
        n.user.push(format!("ChloeCtx is not writing down decisions or rules: {what} ({err}). To fix it: {fix}."));
        n.model.push(format!(
            "ChloeCtx notice: {what} ({err}), so no new decisions, rules or planned work are being written down. Tell the user once, early in your reply: {fix}."
        ));
    }
    if let Some(v) = crate::version::cached(home, i64::MAX).filter(|v| v.behind > 0) {
        if crate::version::notify_due(home) {
            n.user.push(format!("ChloeCtx: a newer version is in the checkout this one was built from: {}.", v.line()));
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(dir: &Path, lines: &[&str]) {
        std::fs::write(dir.join("events.log"), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn a_run_of_failures_since_the_last_success() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(distill_streak(t.path()), DistillStreak::default());
        log(t.path(), &[
            r#"{"ts":1,"event":"distill","kept":2}"#,
            r#"{"ts":2,"event":"distill","error":"model call failed: old"}"#,
            r#"{"ts":3,"event":"distill","kept":1}"#,
            r#"{"ts":4,"event":"distill","error":"model call failed: Invalid API key · Please run /login"}"#,
            r#"{"ts":5,"event":"UserPromptSubmit"}"#,
            r#"{"ts":6,"event":"distill","error":"model call failed: Invalid API key · Please run /login"}"#,
        ]);
        let s = distill_streak(t.path());
        assert_eq!((s.failed, s.ts), (2, 6));
        assert!(is_login_error(&s.error));
        let n = session_notice(t.path(), t.path());
        assert_eq!(n.user.len(), 1, "{n:?}");
        assert!(n.user[0].contains("last 2 distillations failed") && n.user[0].contains("claude auth login"), "{}", n.user[0]);
        assert!(n.model[0].contains("Tell the user"), "{}", n.model[0]);

        // Working again: nothing to say.
        log(t.path(), &[r#"{"ts":6,"event":"distill","error":"x"}"#, r#"{"ts":7,"event":"distill","kept":0}"#]);
        assert!(session_notice(t.path(), t.path()).is_empty());
    }

    #[test]
    fn another_failure_points_at_doctor() {
        let t = tempfile::tempdir().unwrap();
        log(t.path(), &[r#"{"ts":1,"event":"distill","error":"claude: No such file or directory"}"#]);
        let n = session_notice(t.path(), t.path());
        assert!(n.user[0].contains("the last distillation failed") && n.user[0].contains("chloectx doctor"), "{}", n.user[0]);
    }
}
