//! `chloedb-server admin`: the API from a shell, for a script or an agent
//! without MCP. Each command is one request; it prints the answer's JSON
//! on stdout (one line with `--json`, else indented) and exits by what
//! happened, so a script branches without reading the answer:
//!
//! | exit | when |
//! |---|---|
//! | 0 | answered 2xx |
//! | 1 | refused or failed otherwise (the answer's `error_code` says why) |
//! | 2 | refused for who the key is: `unauthenticated`, `scope_refused`, `table_refused`, `reason_required`, `limit_exceeded`, `change_request_required`, `approval_refused` |
//! | 3 | stale: `plan_stale`, `change_not_pending` (look again, file again) |
//! | 4 | retry later: the code is `retryable` (`overloaded`, `unavailable`, ...) |
//! | 5 | the server could not be reached |
//!
//! `--reason` (or `CHLOEDB_REASON`) is sent as `x-chloedb-reason`, and
//! `CHLOEDB_AGENT_MODEL`, `-SESSION`, `-RUN` as `x-chloedb-agent-*`, so
//! the audit log names the agent.

use reqwest::Method;
use serde_json::{json, Value as Json};

/// One request of the API.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub method: Method,
    pub path: String,
    pub body: Option<Json>,
}

impl Call {
    pub fn get(path: impl Into<String>) -> Self {
        Call { method: Method::GET, path: path.into(), body: None }
    }

    pub fn post(path: impl Into<String>, body: Json) -> Self {
        Call { method: Method::POST, path: path.into(), body: Some(body) }
    }
}

/// `call METHOD PATH [BODY]`: a request of any route; BODY is JSON, `-`
/// reads it from stdin, `@file` from a file.
pub fn call_of(method: &str, path: &str, body: Option<&str>) -> Result<Call, String> {
    let method = Method::from_bytes(method.to_ascii_uppercase().as_bytes()).map_err(|_| format!("{method} is not a method"))?;
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    let body = match body {
        None => None,
        Some("-") => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(|e| format!("stdin: {e}"))?;
            Some(serde_json::from_str(&s).map_err(|e| format!("the body on stdin is not JSON: {e}"))?)
        }
        Some(b) if b.starts_with('@') => {
            let s = std::fs::read_to_string(&b[1..]).map_err(|e| format!("{}: {e}", &b[1..]))?;
            Some(serde_json::from_str(&s).map_err(|e| format!("{} is not JSON: {e}", &b[1..]))?)
        }
        Some(b) => Some(serde_json::from_str(b).map_err(|e| format!("the body is not JSON: {e}"))?),
    };
    Ok(Call { method, path, body })
}

/// The change request of a request: `{method, path, body?, reason}`.
pub fn proposed(c: &Call, reason: &str, what: Option<&str>) -> Call {
    let mut b = json!({"method": c.method.as_str(), "path": c.path, "reason": reason});
    if let Some(body) = &c.body {
        b["body"] = body.clone();
    }
    if let Some(w) = what {
        b["what"] = json!(w);
    }
    Call::post("/v1/admin/changes", b)
}

/// The dry run of a request.
pub fn previewed(c: &Call) -> Call {
    let mut b = json!({"method": c.method.as_str(), "path": c.path});
    if let Some(body) = &c.body {
        b["body"] = body.clone();
    }
    Call::post("/v1/admin/changes/preview", b)
}

/// The exit code of an answer (the module's table).
pub fn exit_code(status: u16, answer: &Json) -> i32 {
    if (200..300).contains(&status) {
        return 0;
    }
    let code = answer["error_code"].as_str().unwrap_or_else(|| crate::errors::classify(status, answer["error"].as_str().unwrap_or_default()));
    match code {
        "unauthenticated" | "scope_refused" | "table_refused" | "reason_required" | "limit_exceeded" | "change_request_required" | "approval_refused" => 2,
        "plan_stale" | "change_not_pending" => 3,
        c if crate::errors::lookup(c).is_some_and(|c| c.retryable) || answer["retryable"] == true => 4,
        _ => 1,
    }
}

/// Where and as whom the requests go.
pub struct Target {
    pub url: String,
    pub key: Option<String>,
    pub ca: Option<Vec<u8>>,
    pub reason: Option<String>,
}

/// Sends `c`: the status and the answer (`{"text": ...}` when it is not
/// JSON). `Err` when the server was not reached.
pub async fn send(t: &Target, c: &Call) -> Result<(u16, Json), String> {
    let mut builder = crate::http_client();
    if let Some(pem) = &t.ca {
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(pem).map_err(|e| format!("--ca: {e}"))?);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let url = format!("{}{}", t.url.trim_end_matches('/'), c.path);
    let mut r = client.request(c.method.clone(), &url).header("x-chloedb-app", "chloedb-admin");
    if let Some(k) = &t.key {
        r = r.bearer_auth(k);
    }
    if let Some(why) = t.reason.as_deref().filter(|w| !w.trim().is_empty()) {
        r = r.header("x-chloedb-reason", crate::mcp::encode_header(why.trim()));
    }
    for (env, h) in [("CHLOEDB_AGENT_MODEL", "x-chloedb-agent-model"), ("CHLOEDB_AGENT_SESSION", "x-chloedb-agent-session"), ("CHLOEDB_AGENT_RUN", "x-chloedb-agent-run")] {
        if let Some(v) = std::env::var(env).ok().filter(|v| !v.trim().is_empty()) {
            r = r.header(h, crate::mcp::encode_header(v.trim()));
        }
    }
    if let Some(b) = &c.body {
        r = r.json(b);
    }
    let res = r.send().await.map_err(|e| format!("{url} cannot be reached: {e}"))?;
    let status = res.status().as_u16();
    let text = res.text().await.map_err(|e| format!("{url}: {e}"))?;
    Ok((status, answer_of(status, &text)))
}

/// The answer to print: the body when it is JSON; else the HTTP status and
/// the text, with an `error` naming the status when it is one (a 404 has an
/// empty body, which alone said nothing).
pub fn answer_of(status: u16, text: &str) -> Json {
    serde_json::from_str(text).unwrap_or_else(|_| {
        let mut j = json!({"status": status, "text": text});
        if status >= 400 {
            let reason = reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("error");
            j["error"] = json!(format!("HTTP {status} {reason}"));
        }
        j
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_say_what_to_do() {
        assert_eq!(exit_code(200, &json!({})), 0);
        assert_eq!(exit_code(403, &json!({"error_code": "scope_refused"})), 2);
        assert_eq!(exit_code(403, &json!({"error": "POST /v1/sql needs a key with the admin scope"})), 2);
        assert_eq!(exit_code(409, &json!({"error_code": "plan_stale"})), 3);
        assert_eq!(exit_code(503, &json!({"error_code": "unavailable", "retryable": true})), 4);
        assert_eq!(exit_code(429, &json!({"error": "too many queries running"})), 4);
        assert_eq!(exit_code(400, &json!({"error_code": "query_invalid"})), 1);
    }

    #[test]
    fn an_answer_that_is_not_json_says_its_status() {
        assert_eq!(answer_of(200, r#"{"ok": true}"#), json!({"ok": true}));
        assert_eq!(answer_of(404, ""), json!({"status": 404, "text": "", "error": "HTTP 404 Not Found"}));
        assert_eq!(exit_code(404, &answer_of(404, "")), 1);
        assert_eq!(answer_of(200, "plain"), json!({"status": 200, "text": "plain"}));
    }

    #[test]
    fn calls_are_read_from_words() {
        let c = call_of("post", "v1/admin/plan", Some(r#"{"shards": 3}"#)).unwrap();
        assert_eq!((c.method.as_str(), c.path.as_str(), c.body.clone().unwrap()), ("POST", "/v1/admin/plan", json!({"shards": 3})));
        assert!(call_of("post", "/v1/x", Some("{nope")).is_err());
        let p = proposed(&c, "more room", None);
        assert_eq!(p.body.unwrap(), json!({"method": "POST", "path": "/v1/admin/plan", "body": {"shards": 3}, "reason": "more room"}));
        assert_eq!(previewed(&c).path, "/v1/admin/changes/preview");
    }
}
