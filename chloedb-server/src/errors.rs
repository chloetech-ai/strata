//! Error codes: what an error is, for a program or an agent to act on
//! without reading its sentence.
//!
//! Every error answer carries, beside the sentence it always had
//! (`error`), a stable `error_code` (not `code`, which the released
//! clients read as their own class of error), whether sending the same request again
//! later can succeed (`retryable`), and for most codes a `hint` (what to
//! do next) and the section of the documentation that explains it
//! (`docs`):
//!
//! ```json
//! {"error": "no such table: orders", "error_code": "table_not_found", "retryable": false,
//!  "hint": "list the tables the key may read (GET /v1/tables, or the list_tables tool)",
//!  "docs": "https://chloetech-ai.github.io/chloedb/site/docs/server.html#schemas"}
//! ```
//!
//! An error made with a code ([`ApiError::coded`]) answers that code. The
//! others, made before codes (most of them), are told by their status and
//! their sentence ([`classify`]), so a sentence reworded may move an error
//! to its status's general code (`bad_request`, `conflict`), never to
//! another specific one. [`CODES`] lists every code: a code is never
//! renamed or reused, only added.

use serde_json::{json, Value as Json};

use crate::ApiError;

/// Where the server's reference lives on the website.
const DOCS: &str = "https://chloetech-ai.github.io/chloedb/site/docs/server.html";

/// One code: its name, whether the same request may succeed later, what
/// to do, and the anchor of the server reference that explains it.
pub struct Code {
    pub code: &'static str,
    pub retryable: bool,
    pub hint: &'static str,
    pub docs: &'static str,
}

const fn c(code: &'static str, retryable: bool, hint: &'static str, docs: &'static str) -> Code {
    Code { code, retryable, hint, docs }
}

/// Every code, the general ones (one a status) last.
pub const CODES: &[Code] = &[
    c("unauthenticated", false, "send Authorization: Bearer <key>; a key that expired or was revoked needs a new one", "security"),
    c("scope_refused", false, "the key may not do this: ask for a key with the scope the error names, or file a change request (POST /v1/admin/changes) for a person to approve", "security"),
    c("table_refused", false, "the key is limited to some tables (GET /v1/whoami lists them)", "security"),
    c("reason_required", false, "say why in the x-chloedb-reason header (or reason in the body); the audit log keeps it", "agents-and-administration"),
    c("limit_exceeded", false, "an agent's key may do this for a shorter time or a smaller amount: the error says the most", "agents-and-administration"),
    c("change_request_required", false, "file a change request (POST /v1/admin/changes, or the propose_change tool) for a person to approve", "agents-and-administration"),
    c("approval_refused", false, "a change is approved by a person with an admin key, not by an agent or through MCP", "agents-and-administration"),
    c("change_not_pending", false, "the change request was decided, applied or expired already: GET /v1/admin/changes/<id> says which", "agents-and-administration"),
    c("plan_stale", false, "the cluster changed since the change was planned: file it again to see the new plan", "agents-and-administration"),
    c("table_not_found", false, "list the tables the key may read (GET /v1/tables, or the list_tables tool)", "schemas"),
    c("column_not_found", false, "describe the table (GET /v1/schema?table=..., or the describe_table tool) for its fields", "schemas"),
    c("query_invalid", false, "the statement does not parse or bind: check it with check_sql, and look up the syntax with search_docs", "http-api"),
    c("schema_invalid", false, "the schema is refused as the error says (check_sql, or POST /v1/schema/check, checks one without making it)", "schemas"),
    c("idempotency_conflict", false, "this idempotency key was used for another request: send a new key", "writes-from-many-clients"),
    c("contained", true, "a contain rule limits or refuses these queries for a while (GET /v1/admin/contain); wait, or ask an operator", "who-sends-the-queries-and-containing-them"),
    c("overloaded", true, "the server is busy: send it again after Retry-After seconds", "limits"),
    c("deadline_exceeded", true, "the request ran past its deadline: send it with a larger timeout_ms, or make it cheaper", "deadlines-admission-and-metrics"),
    c("cancelled", false, "someone cancelled the query (DELETE /v1/queries/<id>)", "what-the-server-is-doing-now"),
    c("unavailable", true, "a copy, a shard or a hosted model did not answer: send it again after Retry-After seconds", "failover"),
    c("not_found", false, "nothing has that name or id", "http-api"),
    c("conflict", false, "refused in the state the server is in now, as the error says", "http-api"),
    c("too_large", false, "the request is larger than the server takes: send less at once", "limits"),
    c("unsupported_media_type", false, "send the content type the route takes", "http-api"),
    c("unprocessable", false, "the request is well formed but refused, as the error says", "http-api"),
    c("rate_limited", true, "send it again after Retry-After seconds", "limits"),
    c("not_implemented", false, "this server cannot do that", "http-api"),
    c("bad_request", false, "the request is refused as the error says", "http-api"),
    c("internal", false, "the server failed: the error and the server's logs (x-request-id) say why", "logs"),
    c("upstream", true, "another server failed to answer this one", "logs"),
];

/// The code named `code`.
pub fn lookup(code: &str) -> Option<&'static Code> {
    CODES.iter().find(|c| c.code == code)
}

/// The code of an error made without one: by its status and, for the
/// errors agents meet most, its sentence.
pub fn classify(status: u16, message: &str) -> &'static str {
    let m = message;
    let has = |s: &str| m.contains(s);
    match status {
        401 => "unauthenticated",
        403 if has("touches table") || has("on table") || has("not for a key limited to tables") || has("limited to") => "table_refused",
        403 if has("needs a key with the") || has("scope") => "scope_refused",
        403 => "scope_refused",
        429 if has("contain rule") => "contained",
        429 => "overloaded",
        499 => "cancelled",
        504 => "deadline_exceeded",
        503 => "unavailable",
        413 => "too_large",
        415 => "unsupported_media_type",
        422 if has("idempotency") => "idempotency_conflict",
        422 => "unprocessable",
        501 => "not_implemented",
        502 => "upstream",
        _ if has("no such table") || has("unknown table") => "table_not_found",
        _ if has("no such column") || has("unknown field") || has("no such field") => "column_not_found",
        400 if m.starts_with("query error") || has("parse error") => "query_invalid",
        400 if m.starts_with("schema error") => "schema_invalid",
        400 => "bad_request",
        404 => "not_found",
        409 => "conflict",
        s if s >= 500 => "internal",
        _ => "bad_request",
    }
}

/// The code an error answers: its own, or [`classify`]'s.
pub fn code_of(e: &ApiError) -> &'static str {
    match e.code {
        Some(c) => c,
        None => classify(e.status, &e.message),
    }
}

/// The fields an error's answer carries beside `error`.
pub fn fields(e: &ApiError) -> Json {
    let code = code_of(e);
    match lookup(code) {
        Some(c) => json!({"error_code": c.code, "retryable": c.retryable, "hint": c.hint, "docs": format!("{DOCS}#{}", c.docs)}),
        None => json!({"error_code": code, "retryable": false}),
    }
}

/// An error's whole answer: `{error, code, retryable, hint, docs}`.
pub fn answer(e: &ApiError) -> Json {
    let mut j = fields(e);
    j["error"] = json!(e.message);
    j
}

/// The whole table, for `GET /v1/catalog` and the documentation.
pub fn table() -> Json {
    Json::Array(CODES.iter().map(|c| json!({"code": c.code, "retryable": c.retryable, "hint": c.hint, "docs": format!("{DOCS}#{}", c.docs)})).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_named_in_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for c in CODES {
            assert!(seen.insert(c.code), "{} twice", c.code);
            assert!(c.code.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'), "{}", c.code);
            assert!(!c.hint.is_empty() && !c.docs.is_empty(), "{}", c.code);
        }
    }

    #[test]
    fn every_classified_code_is_listed() {
        let samples = [
            (401, "missing or wrong token"),
            (403, "POST /v1/sql needs a key with the admin scope; \"bot\" has read"),
            (403, "a write touches table x, which the key \"a\" may not: it is limited to y (read)"),
            (429, "refused by contain rule c1 (refuses statement ab, for 30 s more)"),
            (429, "too many queries running; try again"),
            (499, "cancelled"),
            (504, "timeout: the query ran past its deadline"),
            (503, "shard 1: no copy answered in the last second"),
            (400, "query error: no such table: orders"),
            (400, "query error: no such column: price"),
            (400, "query error: expected FROM"),
            (400, "schema error: a field twice"),
            (400, "say which statement"),
            (404, "no such table: orders"),
            (404, "no proposal p9"),
            (409, "a plan is running"),
            (413, "too large"),
            (415, "content type"),
            (422, "this idempotency key was used for another write"),
            (500, "I/O error: disk"),
            (502, "bad gateway"),
            (501, "no sampler"),
        ];
        for (s, m) in samples {
            let code = classify(s, m);
            assert!(lookup(code).is_some(), "{s} {m}: {code} is not listed");
        }
        assert_eq!(classify(400, "query error: no such table: orders"), "table_not_found");
        assert_eq!(classify(404, "no such table: orders"), "table_not_found");
        assert_eq!(classify(400, "query error: no such column: price"), "column_not_found");
        assert_eq!(classify(403, "POST /v1/sql needs a key with the admin scope; \"bot\" has read"), "scope_refused");
        assert_eq!(classify(429, "refused by contain rule c1"), "contained");
    }

    #[test]
    fn a_coded_error_answers_its_code() {
        let e = ApiError::coded(409, "plan_stale", "the cluster changed");
        let j = answer(&e);
        assert_eq!(j["error_code"], "plan_stale");
        // `code` is the released clients' own class of error: never sent.
        assert!(j.get("code").is_none(), "{j}");
        assert_eq!(j["retryable"], false);
        assert_eq!(j["error"], "the cluster changed");
        assert!(j["docs"].as_str().unwrap().starts_with("https://"));
    }
}
