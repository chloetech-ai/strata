//! `chloedb-workbench hook`: what each session's hooks run. It posts the
//! hook's JSON to the server and never fails, so that a session never
//! stops because Workbench did. It prints only what the server answers as
//! the hook's output (`{"hookSpecificOutput": {..., "additionalContext"}}`
//! for `SessionStart`, `UserPromptSubmit` and `PostToolUse`), and nothing
//! when anything goes wrong.

use std::io::Read;
use std::time::Duration;

/// The server a session reports to, from its environment.
pub struct Env {
    pub url: String,
    pub token: String,
    pub agent: String,
}

impl Env {
    pub fn from_env() -> Option<Env> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Some(Env { url: get("CHLOEDB_WORKBENCH_URL")?.trim_end_matches('/').to_string(), token: get("CHLOEDB_WORKBENCH_TOKEN")?, agent: get("CHLOEDB_WORKBENCH_AGENT").unwrap_or_default() })
    }
}

/// An HTTP client for the server on loopback: no proxy, errors as answers.
pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).proxy(None).build().into()
}

pub fn run() {
    let Some(env) = Env::from_env() else { return };
    if env.agent.is_empty() {
        return;
    }
    let mut body = Vec::new();
    if std::io::stdin().take(8 << 20).read_to_end(&mut body).is_err() || body.is_empty() {
        return;
    }
    let res = agent(Duration::from_secs(2))
        .post(format!("{}/api/hook", env.url))
        .header("authorization", format!("Bearer {}", env.token))
        .header("x-workbench-agent", &env.agent)
        .header("content-type", "application/json")
        .send(&body[..]);
    let Ok(mut res) = res else { return };
    if res.status().as_u16() != 200 {
        return;
    }
    let Ok(text) = res.body_mut().with_config().limit(1 << 20).read_to_string() else { return };
    if let Some(out) = output_of(&text) {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{out}").and_then(|_| stdout.flush());
    }
}

/// The hook's output in the server's answer: `output`, when it is a JSON
/// object holding `hookSpecificOutput`.
fn output_of(answer: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(answer).ok()?;
    let out = v.get("output")?;
    out.get("hookSpecificOutput")?.get("hookEventName")?.as_str()?;
    serde_json::to_string(out).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn prints_only_a_hook_output() {
        let o = super::output_of(r#"{"ok":true,"output":{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"x"}}}"#).unwrap();
        assert!(o.starts_with("{\"hookSpecificOutput\""), "{o}");
        assert_eq!(super::output_of(r#"{"ok":true,"output":null}"#), None);
        assert_eq!(super::output_of(r#"{"ok":true}"#), None);
        assert_eq!(super::output_of("not json"), None);
    }
}
