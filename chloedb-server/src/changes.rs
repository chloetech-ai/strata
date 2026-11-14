//! Dry runs and change requests: how an agent changes what it may not
//! change alone ([`crate::ops`]).
//!
//! **A dry run** (`POST /v1/admin/changes/preview {method, path, body?}`,
//! the MCP tool `dry_run`) answers, for any change of any route, in one
//! shape: its tier, what it would do, whether and how it is undone, its
//! blast radius, what to check first, the route's own plan where it has
//! one (a plan of the cluster's shape, a spec, an upgrade; for SQL, the
//! statement bound and the rows it would touch), a `plan_id` and a
//! `fingerprint`. Nothing is changed.
//!
//! **A change request** (`POST /v1/admin/changes {method, path, body?,
//! reason, what?}`, the tool `propose_change`) is a dry run kept for a
//! person: it waits, `pending`, for an hour, beside the on-call agent's
//! proposals (it is one, of action `change`, in the same list and the
//! same command center page), until a person approves, rejects or the
//! asker withdraws it. Approving (`POST /v1/admin/changes/:id/approve`)
//! is refused to an agent ([`crate::ops::is_agent`]: an agent key or
//! role, MCP, the on-call agent, `x-chloedb-agent-*`), so an agent never
//! approves its own or another agent's change; it dry runs the change
//! again, and when its fingerprint differs from the one the request was
//! filed with (the cluster moved: other steps, another table, ten times
//! the rows) it is not applied but fails `plan_stale`, and the agent
//! files it again. Applied, it is the approver's request, with the
//! request's reason, audited as theirs and naming the change request
//! (`x-chloedb-change`).
//!
//! A fingerprint is the SHA-256 of the request and of its plan, the plan
//! without its times and estimates, its numbers from 100 by their order of
//! magnitude (`rows: 41200` reads as `1e4`). When the fingerprints differ,
//! the plans are compared ([`holds`]): a count of rows within twice or half
//! of what it was, or 10 rows apart, is the same, so a table that took
//! a few rows meanwhile does not stale a plan (a `DELETE` of 42 rows that
//! is 44 now), and one that doubled does.

use std::sync::Arc;

use axum::http::{HeaderMap, Method};
use axum::Router;
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};

use crate::mcp::{forward, Caller};
use crate::oncall::{Proposal, PROPOSAL_FOR_MS};
use crate::ops::Tier;
use crate::service::Service;
use crate::telemetry::now_ms;
use crate::ApiError;

/// What a statement risks: a SELECT nothing; writes and table changes 2;
/// a DROP of a table, a DELETE or UPDATE of every row, an ALTER that
/// drops a field, a DROP of a rank profile or function 3. With what it
/// would do, bound against the tables ([`crate::mcp::check_sql`]).
pub fn sql_tier(sql: &str, tables: &[Arc<chloedb::Schema>]) -> (Tier, Json) {
    use chloedb::query::sql::{bind_command, Command, Write};
    let refs: Vec<&chloedb::Schema> = tables.iter().map(|t| &**t).collect();
    let checked = crate::mcp::check_sql(sql, tables).unwrap_or_else(|e| json!({"error": e}));
    let Ok(cmd) = bind_command(sql, &refs, &chloedb::query::Params::default()) else {
        return (0, checked);
    };
    let tier = match &cmd {
        Command::Query | Command::Explain { .. } => 0,
        Command::Write { write: Write::Delete { select }, .. } | Command::Write { write: Write::Update { select, .. }, .. } if select.filter == chloedb::query::logical::Pred::True => 3,
        Command::Write { .. } => 2,
        Command::DropTable { .. } | Command::DropRankProfile { .. } => 3,
        Command::Function(f) if matches!(f, chloedb::query::function::Statement::Drop { .. }) => 3,
        Command::AlterTable { alters, .. } if alters.iter().any(|a| matches!(a, chloedb::schema::Alter::DropField(_))) => 3,
        _ => 2,
    };
    (tier, checked)
}

/// The WHERE of a DELETE or UPDATE, as written: what follows the first
/// `WHERE` outside strings and parentheses, with no `;` after it.
fn where_of(sql: &str) -> Option<&str> {
    let b = sql.as_bytes();
    let (mut i, mut depth, mut quote) = (0, 0i32, None::<u8>);
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if depth == 0 && sql[i..].len() >= 5 && sql[i..i + 5].eq_ignore_ascii_case("where") => {
                    let before = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
                    let after = b.get(i + 5).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
                    if before && after {
                        return Some(sql[i + 5..].trim().trim_end_matches(';').trim()).filter(|w| !w.is_empty());
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// What a request does, if it can be undone, how, and what clients feel
/// meanwhile: for the routes an operator changes most; others are told
/// by their tier.
fn notes(method: &Method, route: &str, tier: Tier) -> (String, bool, Option<&'static str>, Option<&'static str>) {
    let m = method.as_str();
    let (what, undo, blast): (&str, Option<&str>, Option<&str>) = match (m, route) {
        ("POST", "/v1/admin/plan/apply") => ("apply a plan of the cluster's shape, its steps in order, in the background", Some("a plan back to the shape before (POST /v1/admin/plan with the old copies)"), Some("rows are copied between servers while reads and writes go on; a primary handed over refuses writes for a moment")),
        ("POST", "/v1/admin/kube") => ("change the StrataCluster's spec; the operator then makes the change", Some("POST /v1/admin/kube {\"rollback\": <the version before>}"), Some("as the spec's plan says (POST /v1/admin/kube/plan)")),
        ("POST", "/v1/admin/kube/upgrade") => ("roll the cluster out to another image, one server at a time", Some("an upgrade back to the image before; a failed rollout rolls back by itself (spec.rollout.onFailure)"), Some("each server restarts in turn while the others serve")),
        ("POST", "/v1/admin/kube/resume") => ("lift the hold a gate put on the operator's change", None, Some("the change goes on")),
        ("DELETE", "/v1/admin/shards/:i") => ("remove a shard from the cluster map", None, Some("its rows leave the cluster's answers unless they were moved off first")),
        ("POST", "/v1/admin/shards") => ("add a shard to the cluster map", Some("remove it again while it holds no slots"), None),
        ("POST", "/v1/admin/shards/:i/replicas") => ("add a copy to a shard", Some("remove the copy (DELETE the same route)"), Some("the new copy is sent the shard's rows")),
        ("DELETE", "/v1/admin/shards/:i/replicas") => ("remove a copy from a shard", Some("add the copy again; it is sent the rows anew"), Some("the shard has one copy fewer to fail over to")),
        ("POST", "/v1/admin/shards/:i/promote") | ("POST", "/v1/admin/promote") => ("make a replica the primary of a new term", None, Some("writes pause while the primary changes; with force, transactions only the old primary held are dropped")),
        ("POST", "/v1/admin/rebalance") | ("POST", "/v1/admin/moves") => ("move slots between shards", Some("move them back"), Some("rows are copied; each slot's writes pause a moment at its switch")),
        ("POST", "/v1/admin/vacuum") => ("rewrite segments, dropping deleted rows and old versions for good", None, Some("disk and CPU while it runs; snapshots older than it can no longer be read")),
        ("POST", "/v1/admin/hold-writes") => ("hold every write of the cluster", Some("release the hold"), Some("every write is refused (503) while held")),
        ("POST", "/v1/admin/follow/stop") => ("stop following the source cluster: this cluster takes its own writes", None, Some("writes made on the source after it are not copied")),
        ("POST", "/v1/admin/follow/promote") => ("promote this cluster over the one it follows", None, Some("clients must be sent to this cluster")),
        ("POST", "/v1/admin/keys") => ("make an API key", Some("revoke it (DELETE /v1/admin/keys/<id>)"), None),
        ("DELETE", "/v1/admin/keys/:id") => ("revoke an API key: its clients are refused at once, on every server", None, Some("every client that sends it gets 401")),
        ("POST", "/v1/admin/keys/:id/rotate") => ("make a new key in place of one, the old one ending after a grace", None, Some("clients must move to the new key before the grace ends")),
        ("PUT", "/v1/alerts/rules") => ("replace the alert rules", Some("PUT the rules before (GET /v1/alerts lists them)"), Some("alerts fire by the new rules from now")),
        ("PUT", "/v1/alerts/routes") => ("replace where notifications go", Some("PUT the routes before"), Some("notifications go to the new routes from now")),
        ("PUT", "/v1/synonyms") => ("replace the server's synonyms", Some("PUT the list before (GET /v1/synonyms)"), Some("searches expand by the new list from now")),
        ("DELETE", "/v1/admin/tenants/:id") => ("delete a tenant's database from the object store", None, Some("its rows are gone")),
        ("POST", "/v1/admin/cloud/clusters") => ("make a cluster in a cloud account", Some("delete it (POST /v1/admin/cloud/clusters/<id>/delete)"), Some("the account is billed until it is deleted")),
        ("POST", "/v1/admin/cloud/clusters/:id/delete") | ("POST", "/v1/admin/cloud/found/delete") => ("delete a cloud cluster and its volumes", None, Some("its servers and their data are gone")),
        ("POST", "/v1/relevance/proposals/:id/apply") => ("replace a table's rank profile by the autopilot's proposal", Some("apply the profile before again"), Some("searches rank by the new profile from now")),
        _ => ("", None, None),
    };
    let what = if what.is_empty() { format!("{m} {route}") } else { what.to_string() };
    let reversible = match (undo, tier) {
        (Some(_), _) => true,
        (None, t) => t <= 1,
    };
    (what, reversible, undo, blast)
}

/// Whether a plan's key is left out of its comparison: times and
/// estimates.
fn ignored(k: &str) -> bool {
    k.ends_with("_ms") || k.ends_with("_s") || k.starts_with("took") || k.starts_with("estimate") || k.contains("seconds") || k == "fingerprint" || k == "plan_id" || k == "now" || k == "ts"
}

/// A plan as it is fingerprinted: no times or estimates, numbers from 100
/// by their order of magnitude, no fractions.
fn canonical(j: &Json) -> Json {
    match j {
        Json::Object(m) => Json::Object(m.iter().filter(|(k, _)| !ignored(k)).map(|(k, v)| (k.clone(), canonical(v))).collect()),
        Json::Array(a) => Json::Array(a.iter().map(canonical).collect()),
        Json::Number(n) => match n.as_u64() {
            Some(x) if x < 100 => json!(x),
            Some(x) => json!(format!("1e{}", (x as f64).log10().floor() as u32)),
            None if n.is_i64() => json!(n),
            None => Json::Null,
        },
        other => other.clone(),
    }
}

/// Whether a plan's key holds a count of rows (or documents, records,
/// bytes) rather than a name or a number of something else (a shard, a
/// tier, a version).
fn is_count(k: &str) -> bool {
    matches!(k, "rows" | "n" | "count" | "docs" | "records" | "bytes") || ["_rows", "_docs", "_count", "_records", "_bytes"].iter().any(|s| k.ends_with(s))
}

/// Whether plan `now` is plan `was` still, for an approval: the same but
/// for times and estimates, its counts within twice or half of what they
/// were, or 10 apart (a table taking writes meanwhile; [`is_count`]), its
/// other numbers as the fingerprint reads them ([`canonical`]).
fn holds(key: &str, was: &Json, now: &Json) -> bool {
    match (was, now) {
        (Json::Object(a), Json::Object(b)) => {
            let ka: std::collections::BTreeSet<&String> = a.keys().filter(|k| !ignored(k)).collect();
            let kb: std::collections::BTreeSet<&String> = b.keys().filter(|k| !ignored(k)).collect();
            ka == kb && ka.iter().all(|k| holds(k, &a[k.as_str()], &b[k.as_str()]))
        }
        (Json::Array(a), Json::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| holds(key, x, y)),
        (Json::Number(x), Json::Number(y)) if is_count(key) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => {
                let (lo, hi) = if x <= y { (x, y) } else { (y, x) };
                hi - lo <= 10.0 || (lo > 0.0 && hi < 2.0 * lo)
            }
            _ => false,
        },
        (a, b) => canonical(a) == canonical(b),
    }
}

/// The fingerprint of a request and its plan (the module's last part).
pub fn fingerprint(method: &str, path: &str, body: &Json, plan: &Json) -> String {
    let mut h = Sha256::new();
    h.update(method.as_bytes());
    h.update(b" ");
    h.update(path.as_bytes());
    h.update(b"\n");
    h.update(body.to_string().as_bytes());
    h.update(b"\n");
    h.update(canonical(plan).to_string().as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `{method, path, body?}` of a request asked about.
pub struct Asked {
    pub method: Method,
    pub path: String,
    pub route: &'static str,
    pub body: Json,
}

pub fn asked(b: &Json) -> Result<Asked, ApiError> {
    let m = b["method"].as_str().map(str::to_ascii_uppercase).ok_or_else(|| ApiError::bad("say the request's method: POST, PUT, PATCH or DELETE"))?;
    let method = match m.as_str() {
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        "DELETE" => Method::DELETE,
        "GET" | "HEAD" => return Err(ApiError::bad("a read changes nothing: call it")),
        _ => return Err(ApiError::bad(format!("{m} is not a method of the API"))),
    };
    let path = b["path"].as_str().map(str::trim).ok_or_else(|| ApiError::bad("say the request's path, as /v1/admin/plan/apply"))?.to_string();
    if !path.starts_with("/v1/") || path.contains("..") || path.chars().any(|c| c.is_whitespace() || c.is_control()) || path.len() > 2000 {
        return Err(ApiError::bad("the path is one of this server's routes, from /v1/ (GET /v1/catalog lists them)"));
    }
    let route = crate::catalog::route_of(&path).ok_or_else(|| ApiError::coded(404, "not_found", format!("no route matches {path} (GET /v1/catalog lists them)")))?;
    let known = crate::catalog::routes().iter().any(|r| r.path == route && r.methods.contains(&method));
    if !known {
        return Err(ApiError::coded(404, "not_found", format!("{route} does not take {method} (GET /v1/catalog lists its methods)")));
    }
    // A change request does not approve, reject or file change requests,
    // nor the on-call agent's proposals.
    if route.starts_with("/v1/admin/changes") || route.starts_with("/v1/admin/oncall/proposals") || route == "/v1/mcp" {
        return Err(ApiError::bad("a change request does not decide or file change requests: a person does that"));
    }
    // Nor does it carry a credential, or make one (audit X-D1): it is kept
    // on disk and read by every role key, with the answer it got.
    if crate::ops::carries_credentials(route) {
        return Err(ApiError::coded(403, "scope_refused", format!("{method} {route} carries or answers a credential (a key's secret, another cluster's, a connector's or a cloud's): a change request does not, since every role key reads it. A person calls it with their own key")));
    }
    let body = b.get("body").cloned().unwrap_or(Json::Null);
    if let Some(field) = secret_in(&body) {
        return Err(ApiError::coded(400, "bad_request", format!("the body holds {field:?}, a credential: a change request does not carry one, since every role key reads it. A person sends it with their own key")));
    }
    Ok(Asked { method, path, route, body })
}

/// Whether a field named `name` holds a credential: a secret, a token, a
/// password or a private key, by its name.
fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase().replace(['-', '.', ' '], "_");
    let parts: Vec<&str> = n.split('_').filter(|p| !p.is_empty()).collect();
    const WORDS: &[&str] = &["secret", "secrets", "token", "password", "passwd", "passphrase", "credential", "credentials", "authorization", "cookie", "bearer", "apikey"];
    // Of two words: `private_key`, `api_key`, `access_key`.
    const BEFORE_KEY: &[&str] = &["private", "api", "access", "signing"];
    parts.iter().any(|p| WORDS.contains(p)) || parts.windows(2).any(|w| w[1] == "key" && BEFORE_KEY.contains(&w[0]))
}

/// The first field of `j`, at any depth, whose name says it holds a
/// credential ([`secret_name`]) and whose value is not empty.
pub fn secret_in(j: &Json) -> Option<String> {
    match j {
        Json::Object(m) => m.iter().find_map(|(k, v)| {
            let empty = v.is_null() || v.as_str().is_some_and(|s| s.is_empty()) || v.as_bool() == Some(false);
            if secret_name(k) && !empty {
                Some(k.clone())
            } else {
                secret_in(v)
            }
        }),
        Json::Array(a) => a.iter().find_map(secret_in),
        _ => None,
    }
}

/// `j` with the value of every field whose name says it holds a
/// credential ([`secret_name`]) replaced by `"(hidden)"`: what a change
/// request keeps of a body and of the answer it got (audit X-D1).
pub fn scrub(j: &Json) -> Json {
    match j {
        Json::Object(m) => Json::Object(
            m.iter()
                .map(|(k, v)| {
                    let hide = secret_name(k) && !(v.is_null() || v.is_boolean() || v.as_str().is_some_and(|s| s.is_empty()));
                    (k.clone(), if hide { json!("(hidden)") } else { scrub(v) })
                })
                .collect(),
        ),
        Json::Array(a) => Json::Array(a.iter().map(scrub).collect()),
        _ => j.clone(),
    }
}

/// The dry run of request `a`, sent by `caller` (the module's first part).
pub async fn preview(svc: &Arc<Service>, router: &Router, caller: &Caller<'_>, a: &Asked) -> Result<Json, ApiError> {
    let mut tier = crate::ops::tier(&a.method, a.route);
    let mut plan = Json::Null;
    let mut affects = Json::Null;
    let mut would: Option<String> = None;
    let mut preflight = Json::Null;
    let mut undo_sql: Option<&'static str> = None;
    let fail = |e: String| -> Json { json!({"error": e}) };
    match (a.method.as_str(), a.route) {
        ("POST", "/v1/admin/plan/apply") => {
            plan = match a.body.get("steps") {
                Some(_) => a.body.clone(),
                None => forward(router, caller, Method::POST, "/v1/admin/plan", Some(a.body.clone())).await.unwrap_or_else(fail),
            };
        }
        ("POST", "/v1/admin/kube") => plan = forward(router, caller, Method::POST, "/v1/admin/kube/plan", Some(a.body.clone())).await.unwrap_or_else(fail),
        ("POST", "/v1/admin/kube/upgrade") => {
            let mut b = if a.body.is_object() { a.body.clone() } else { json!({}) };
            b["plan"] = json!(true);
            plan = forward(router, caller, Method::POST, "/v1/admin/kube/upgrade", Some(b)).await.unwrap_or_else(fail);
        }
        ("POST", "/v1/sql") => {
            let sql = a.body["sql"].as_str().ok_or_else(|| ApiError::bad("a statement for /v1/sql is the body's \"sql\""))?;
            let tables = svc.tables_listed().await;
            let (t, checked) = sql_tier(sql, &tables);
            tier = t;
            would = checked["statement"].as_str().map(|s| format!("{s} {}", checked["table"].as_str().or(checked["tables"][0].as_str()).unwrap_or_default()).trim().to_string());
            // The rows it touches: those its WHERE matches, or the
            // table's.
            let count = match checked["statement"].as_str() {
                Some("delete" | "update") => checked["table"].as_str().map(|t| match where_of(sql) {
                    Some(w) => format!("SELECT COUNT(*) AS n FROM {t} WHERE {w}"),
                    None => format!("SELECT COUNT(*) AS n FROM {t}"),
                }),
                Some("drop table") => checked["tables"][0].as_str().map(|t| format!("SELECT COUNT(*) AS n FROM {t}")),
                _ => None,
            };
            if let Some(q) = count {
                if let Ok(r) = forward(router, caller, Method::POST, "/v1/query", Some(json!({"sql": q, "timeout_ms": 10_000}))).await {
                    let n = r["rows"][0].get(0).or_else(|| r["rows"][0].get("n")).cloned().unwrap_or(Json::Null);
                    affects = json!({"table": checked["table"].as_str().or(checked["tables"][0].as_str()), "rows": n});
                }
            }
            undo_sql = match checked["statement"].as_str() {
                Some("drop table") => None,
                Some("delete" | "update") => Some("none in SQL: restore the rows from a checkpoint (POST /v1/admin/checkpoint before it) or rewrite them"),
                Some("create table") => Some("DROP TABLE"),
                Some("insert") => Some("DELETE the rows it wrote"),
                _ => None,
            };
            plan = checked;
        }
        _ => {}
    }
    if let Some(p) = plan.get("preflight") {
        preflight = p.clone();
    }
    let (what, mut reversible, mut undo, blast) = notes(&a.method, a.route, tier);
    if a.route == "/v1/sql" {
        undo = undo_sql;
        reversible = matches!(plan["statement"].as_str(), Some("create table" | "insert"));
    }
    let fp = fingerprint(a.method.as_str(), &a.path, &a.body, &json!({"plan": plan, "affects": affects}));
    Ok(json!({
        "dry_run": true,
        "request": {"method": a.method.as_str(), "path": a.path, "body": a.body},
        "route": a.route,
        "tier": tier,
        "would": would.unwrap_or(what),
        "reversible": reversible,
        "undo": undo,
        "blast": blast,
        "affects": affects,
        "preflight": preflight,
        "plan": plan,
        "plan_id": format!("pl_{}", &fp[..12]),
        "fingerprint": fp,
        "alone": match tier { 0 => "anyone of the route's scope", 1 => "an ops or admin key, with a reason", _ => "a person with an admin key; an agent files a change request" },
        "expires_ms": now_ms() + PROPOSAL_FOR_MS,
    }))
}

/// Files change request `a` with its reason (the module's second part):
/// the proposal kept, pending.
pub async fn file(svc: &Arc<Service>, router: &Router, caller: &Caller<'_>, by: &str, agent: bool, a: Asked, reason: &str, what: Option<&str>) -> Result<Json, ApiError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(ApiError::coded(400, "reason_required", "a change request says why: `reason`, a sentence the person who approves it reads"));
    }
    let dry = preview(svc, router, caller, &a).await?;
    if dry["tier"] == 0 {
        return Err(ApiError::bad("that request changes nothing: call it"));
    }
    let t = now_ms();
    let what = what.map(str::trim).filter(|w| !w.is_empty()).map(String::from).unwrap_or_else(|| dry["would"].as_str().unwrap_or_default().to_string());
    let p = Proposal {
        id: String::new(),
        run: String::new(),
        created_ms: t,
        expires_ms: t + PROPOSAL_FOR_MS,
        asked_by: by.to_string(),
        action: "change".into(),
        what: crate::obs::truncate(&what, 300).to_string(),
        why: crate::obs::truncate(reason, 1000).to_string(),
        method: a.method.to_string(),
        path: a.path.clone(),
        body: (!a.body.is_null()).then(|| a.body.clone()),
        state: "pending".into(),
        decided_by: None,
        decided_ms: None,
        result: None,
        tier: dry["tier"].as_u64().map(|t| t as u8),
        plan: Some(dry.clone()),
        fingerprint: dry["fingerprint"].as_str().map(String::from),
        agent,
    };
    svc.oncall.ready(svc);
    let id = svc.oncall.add_change(p);
    tracing::info!(change = %id, by, tier = dry["tier"].as_u64().unwrap_or(0), "change request filed: {what}");
    Ok(svc.oncall.get(&id).unwrap_or(Json::Null))
}

/// Whether the change request `p` still holds: its request dry run again
/// by the approver, and the fingerprints compared, then the plans
/// ([`holds`]: counts within twice or half, or 10 apart). `Err` with the
/// plan now when the cluster moved.
pub async fn still_holds(svc: &Arc<Service>, router: &Router, caller: &Caller<'_>, p: &Proposal) -> Result<(), Json> {
    let Some(was) = &p.fingerprint else { return Ok(()) };
    let Ok(method) = Method::from_bytes(p.method.as_bytes()) else { return Ok(()) };
    let Some(route) = crate::catalog::route_of(&p.path) else { return Ok(()) };
    let a = Asked { method, path: p.path.clone(), route, body: p.body.clone().unwrap_or(Json::Null) };
    match preview(svc, router, caller, &a).await {
        Ok(now) if now["fingerprint"].as_str() == Some(was.as_str()) => Ok(()),
        // Its rows counted again on a table that takes writes.
        Ok(now) if p.plan.as_ref().is_some_and(|old| ["plan", "affects"].iter().all(|k| holds(k, &old[*k], &now[*k]))) => Ok(()),
        Ok(now) => Err(now),
        Err(e) => Err(json!({"error": e.message})),
    }
}

/// The headers an approved change is applied with: the approver's, its
/// reason, the change request it applies, and an app that is no agent's.
pub fn applying(headers: &HeaderMap, id: &str, why: &str) -> HeaderMap {
    let mut h = headers.clone();
    let reason = format!("{} {id}: {why}", if id.starts_with('c') { "change request" } else { "on-call proposal" });
    for (k, v) in [("x-strata-reason", crate::mcp::encode_header(&crate::obs::truncate(&reason.replace(['\r', '\n'], " "), 500))), ("x-strata-change", id.to_string()), ("x-strata-app", "approved-change".to_string())] {
        if let Ok(v) = axum::http::HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    }
    for k in ["x-strata-agent-model", "x-strata-agent-session", "x-strata-agent-run", "x-chloedb-agent-model", "x-chloedb-agent-session", "x-chloedb-agent-run"] {
        h.remove(k);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_found_and_hidden() {
        assert_eq!(secret_in(&json!({"name": "x", "auth": {"api_key": "k"}})), Some("api_key".into()));
        assert_eq!(secret_in(&json!([{"Password": "p"}])), Some("Password".into()));
        assert_eq!(secret_in(&json!({"x-chloedb-token": "t"})), Some("x-chloedb-token".into()));
        assert_eq!(secret_in(&json!({"secret_access_key": "s"})), Some("secret_access_key".into()));
        // Words that hold no credential, and empty values.
        assert_eq!(secret_in(&json!({"max_tokens": 10, "keys": ["a"], "key": "id", "tokenizer": "en", "secret": "", "token": null})), None);
        let made = json!({"key": {"id": "k1", "name": "a"}, "secret": "sk_k1_abc", "old": {"token": "t"}, "note": "made"});
        assert_eq!(scrub(&made), json!({"key": {"id": "k1", "name": "a"}, "secret": "(hidden)", "old": {"token": "(hidden)"}, "note": "made"}));
        assert_eq!(scrub(&json!([1, "x"])), json!([1, "x"]));
    }

    #[test]
    fn the_where_of_a_statement() {
        assert_eq!(where_of("DELETE FROM t WHERE a = 'x where y' AND b > 2;"), Some("a = 'x where y' AND b > 2"));
        assert_eq!(where_of("UPDATE t SET note = 'where' WHERE id IN (SELECT id FROM u WHERE z)"), Some("id IN (SELECT id FROM u WHERE z)"));
        assert_eq!(where_of("DELETE FROM t"), None);
        assert_eq!(where_of("DELETE FROM somewhere"), None);
    }

    #[test]
    fn fingerprints_ignore_times_and_small_drift_but_not_tenfold() {
        let body = json!({"sql": "DELETE FROM t WHERE a"});
        let f = |rows: u64, took: f64| fingerprint("POST", "/v1/sql", &body, &json!({"statement": "delete", "rows": rows, "took_ms": took, "estimate_s": took}));
        assert_eq!(f(41_200, 1.0), f(43_900, 9.0));
        assert_ne!(f(41_200, 1.0), f(412_000, 1.0));
        assert_ne!(f(3, 1.0), f(4, 1.0));
        assert_ne!(fingerprint("POST", "/v1/sql", &json!({"sql": "DROP TABLE a"}), &Json::Null), fingerprint("POST", "/v1/sql", &json!({"sql": "DROP TABLE b"}), &Json::Null));
    }

    #[test]
    fn plans_hold_while_their_counts_drift_a_little_but_not_twofold() {
        let dry = |rows: u64, shard: u64| json!({"plan": {"statement": "delete", "table": "products", "shard": shard}, "affects": {"table": "products", "rows": rows}, "took_ms": rows});
        let same = |a: &Json, b: &Json| ["plan", "affects"].iter().all(|k| holds(k, &a[*k], &b[*k]));
        // A few rows more or less on a busy table: small counts too.
        assert!(same(&dry(42, 1), &dry(44, 1)));
        assert!(same(&dry(3, 1), &dry(9, 1)));
        assert!(same(&dry(0, 1), &dry(4, 1)));
        assert!(same(&dry(41_200, 1), &dry(43_900, 1)));
        assert!(same(&dry(99, 1), &dry(101, 1)), "no edge at an order of magnitude");
        // Twice or half the rows, another shard or table: stale.
        assert!(!same(&dry(52, 1), &dry(120, 1)));
        assert!(!same(&dry(41_200, 1), &dry(9_000, 1)));
        assert!(!same(&dry(42, 1), &dry(42, 2)), "a shard is a name, not a count");
        let mut other = dry(42, 1);
        other["affects"]["table"] = json!("orders");
        assert!(!same(&dry(42, 1), &other));
    }

    #[test]
    fn requests_asked_about_are_routes_that_change() {
        assert_eq!(asked(&json!({"method": "post", "path": "/v1/admin/shards/2/drain"})).unwrap().route, "/v1/admin/shards/:i/drain");
        assert!(asked(&json!({"method": "GET", "path": "/v1/tables"})).is_err());
        assert!(asked(&json!({"method": "POST", "path": "https://elsewhere/v1/sql"})).is_err());
        assert!(asked(&json!({"method": "POST", "path": "/v1/admin/changes/c1/approve"})).is_err());
        assert!(asked(&json!({"method": "POST", "path": "/v1/admin/oncall/proposals/p1/approve"})).is_err());
        assert!(asked(&json!({"method": "POST", "path": "/v1/../admin/keys"})).is_err());
        assert_eq!(asked(&json!({"method": "POST", "path": "/v1/nothing"})).err().unwrap().code(), "not_found");
    }
}
