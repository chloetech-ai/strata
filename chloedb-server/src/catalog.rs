//! The catalog of routes (`GET /v1/catalog`): every route of the HTTP API
//! with the scope it needs, what it risks ([`crate::ops::tier`]), whether
//! it can be dry run, and the MCP tool that calls it; and every error code
//! ([`crate::errors`]). An agent learns from it what it may call, and how
//! to make sense of what it is answered.
//!
//! The routes are read from the router's own source (`http.rs`, compiled
//! in), so the catalog cannot list a route the server does not have, nor
//! miss one it has.

use std::sync::OnceLock;

use axum::http::Method;
use serde_json::{json, Value as Json};

const SOURCE: &str = include_str!("http.rs");

/// One route of the API and the methods it answers.
#[derive(Clone, Debug)]
pub struct Route {
    pub path: &'static str,
    pub methods: Vec<Method>,
}

/// Every route, in the router's order.
pub fn routes() -> &'static [Route] {
    static ALL: OnceLock<Vec<Route>> = OnceLock::new();
    ALL.get_or_init(|| parse(SOURCE))
}

fn parse(src: &'static str) -> Vec<Route> {
    let mut out: Vec<Route> = Vec::new();
    let mut rest = src;
    while let Some(at) = rest.find(".route(\"") {
        rest = &rest[at + 8..];
        let Some(end) = rest.find('"') else { break };
        let path = &rest[..end];
        // What follows, to the end of the call: the line, or the next
        // `.route(` when a call spans lines.
        let tail = &rest[end..];
        let stop = tail.find("\n        .route(").or_else(|| tail.find('\n')).unwrap_or(tail.len());
        let call = &tail[..stop];
        let mut methods = Vec::new();
        for (word, m) in [("get(", Method::GET), ("post(", Method::POST), ("put(", Method::PUT), ("patch(", Method::PATCH), ("delete(", Method::DELETE)] {
            if call.contains(word) {
                methods.push(m);
            }
        }
        if call.contains("any(") {
            methods = vec![Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE];
        }
        if !path.starts_with('/') || methods.is_empty() {
            continue;
        }
        match out.iter_mut().find(|r| r.path == path) {
            Some(r) => {
                for m in methods {
                    if !r.methods.contains(&m) {
                        r.methods.push(m);
                    }
                }
            }
            None => out.push(Route { path, methods }),
        }
    }
    out
}

/// The route a request's path matches (`/v1/admin/shards/2/drain` is
/// `/v1/admin/shards/:i/drain`), the query string left out.
pub fn route_of(path: &str) -> Option<&'static str> {
    let path = path.split('?').next().unwrap_or(path);
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let mut best: Option<(&'static str, usize)> = None;
    for r in routes() {
        let pat: Vec<&str> = r.path.split('/').collect();
        let rest = pat.last().is_some_and(|p| p.starts_with('*'));
        let fixed_len = if rest { pat.len() - 1 } else { pat.len() };
        if (rest && segs.len() <= fixed_len) || (!rest && segs.len() != pat.len()) {
            continue;
        }
        let mut fixed = 0;
        let ok = pat[..fixed_len].iter().zip(&segs).all(|(p, s)| match p.strip_prefix(':') {
            Some(_) => !s.is_empty(),
            None => {
                fixed += 1;
                p == s
            }
        });
        if ok && best.is_none_or(|(_, f)| fixed > f) {
            best = Some((r.path, fixed));
        }
    }
    best.map(|(p, _)| p)
}

/// The MCP tools that call a route by a method ([`crate::mcp`]).
const TOOLS: &[(&str, &str, &str)] = &[
    ("GET", "/v1/tables", "list_tables"),
    ("GET", "/v1/schema", "describe_table"),
    ("POST", "/v1/query", "query"),
    ("POST", "/v1/explain", "explain"),
    ("POST", "/v1/sql", "execute"),
    ("POST", "/v1/write", "write"),
    ("POST", "/v1/kv/get", "get_rows"),
    ("GET", "/v1/tables/:table/paths", "json_paths"),
    ("GET", "/v1/models", "list_models"),
    ("POST", "/v1/judge", "judge"),
    ("GET", "/v1/feedback", "list_feedback"),
    ("GET", "/v1/feedback/:id", "get_feedback"),
    ("PATCH", "/v1/feedback/:id", "update_feedback"),
    ("GET", "/v1/whoami", "whoami"),
    ("GET", "/v1/health", "health"),
    ("GET", "/v1/health/fleet", "health"),
    ("GET", "/v1/cluster", "cluster"),
    ("GET", "/v1/load", "load"),
    ("GET", "/v1/system", "system"),
    ("GET", "/v1/alerts", "alerts"),
    ("GET", "/v1/alerts/:rule", "alerts"),
    ("GET", "/v1/events", "events"),
    ("GET", "/v1/queries", "queries"),
    ("GET", "/v1/queries/slow", "queries"),
    ("GET", "/v1/queries/recent", "queries"),
    ("GET", "/v1/queries/stats", "queries"),
    ("GET", "/v1/logs", "logs"),
    ("GET", "/v1/timeseries", "timeseries"),
    ("GET", "/v1/admin/contain", "contain_rules"),
    ("GET", "/v1/admin/audit", "audit"),
    ("GET", "/v1/catalog", "catalog"),
    ("GET", "/v1/runbooks", "runbook"),
    ("GET", "/v1/admin/jobs", "jobs"),
    ("POST", "/v1/admin/plan", "plan_reshape"),
    ("POST", "/v1/admin/kube/plan", "plan_kube"),
    ("POST", "/v1/admin/changes/preview", "dry_run"),
    ("POST", "/v1/admin/changes", "propose_change"),
    ("GET", "/v1/admin/changes", "list_changes"),
    ("GET", "/v1/admin/changes/:id", "get_change"),
    ("POST", "/v1/admin/contain", "contain"),
    ("DELETE", "/v1/admin/contain/:id", "remove_contain"),
    ("DELETE", "/v1/queries/:id", "cancel_query"),
    ("POST", "/v1/alerts/:rule/silence", "silence_alert"),
    ("POST", "/v1/alerts/:rule/ack", "ack_alert"),
    ("POST", "/v1/events/notes", "note"),
];

/// Routes with a dry run of their own, beside the generic one of
/// `POST /v1/admin/changes/preview`: the route that plans them.
pub const PLANS: &[(&str, &str, &str)] = &[
    ("POST", "/v1/admin/plan/apply", "POST /v1/admin/plan"),
    ("POST", "/v1/admin/kube", "POST /v1/admin/kube/plan"),
    ("POST", "/v1/admin/kube/upgrade", "the same route with {\"plan\": true}"),
    ("POST", "/v1/import", "the same route with ?plan=true (SQLite and Parquet)"),
    ("POST", "/v1/sql", "POST /v1/admin/changes/preview (rows a DELETE or UPDATE changes, a table's rows a DROP loses)"),
    ("POST", "/v1/admin/cloud/clusters", "POST /v1/admin/aws/plan or /v1/admin/digitalocean/plan"),
];

/// The experimental features: routes by the start of their path, the
/// feature's name and what it is. Their API, settings and stored state
/// carry no compatibility promise yet and may change in any release
/// (docs/compatibility.md, "Experimental features"). Each answer of one
/// of these routes says so in [`EXPERIMENTAL_HEADER`], and the catalog
/// marks the route. A prefix ends where a path segment does (`/v1/admin/hub`
/// is `/v1/admin/hub` and `/v1/admin/hub/...`, not `/v1/admin/hubs`).
pub const EXPERIMENTAL: &[(&str, &str, &str)] = &[
    ("/v1/admin/cloud", "cloud", "clusters made on AWS or DigitalOcean from the command center"),
    ("/v1/admin/aws", "cloud", "clusters made on AWS or DigitalOcean from the command center"),
    ("/v1/admin/digitalocean", "cloud", "clusters made on AWS or DigitalOcean from the command center"),
    ("/v1/admin/hub", "hub", "the hub: a book of other clusters, and calls forwarded to them"),
    ("/c", "hub", "the hub: a book of other clusters, and calls forwarded to them"),
    ("/v1/admin/kube/autoscale", "autoscale", "the operator's autoscaler and its rules"),
    ("/v1/admin/autoscale", "autoscale", "the operator's autoscaler and its rules"),
    ("/v1/admin/oncall", "oncall", "the on-call agent: a model that looks into an alert and proposes actions"),
];

/// The header that names the experimental feature an answer comes from.
pub const EXPERIMENTAL_HEADER: &str = "x-chloedb-experimental";

/// The experimental feature a path (or a route's pattern) belongs to.
pub fn experimental(path: &str) -> Option<&'static str> {
    let path = path.split('?').next().unwrap_or(path);
    EXPERIMENTAL.iter().find(|(p, _, _)| path.strip_prefix(p).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))).map(|e| e.1)
}

/// The catalog's entries, of routes whose path holds `q` when given.
pub fn entries(q: Option<&str>) -> Vec<Json> {
    let q = q.map(str::to_ascii_lowercase).filter(|q| !q.is_empty());
    let mut out = Vec::new();
    for r in routes() {
        if q.as_deref().is_some_and(|q| !r.path.to_ascii_lowercase().contains(q)) {
            continue;
        }
        for m in &r.methods {
            let scope = crate::http::route_scope(m, r.path);
            let tier = crate::ops::tier(m, r.path);
            let tool = TOOLS.iter().find(|(tm, tp, _)| *tm == m.as_str() && *tp == r.path).map(|t| t.2);
            let plan = PLANS.iter().find(|(pm, pp, _)| *pm == m.as_str() && *pp == r.path).map(|p| p.2);
            let mut j = json!({
                "method": m.as_str(),
                "path": r.path,
                "scope": scope.name(),
                "tier": tier,
                "reads": crate::ops::is_read(m, r.path),
                "roles": {
                    "propose": crate::ops::role_may(Some(crate::keys::Role::Propose), m, r.path) || scope <= crate::keys::Scope::Read,
                    "ops": crate::ops::role_may(Some(crate::keys::Role::Ops), m, r.path) || scope <= crate::keys::Scope::Read,
                },
            });
            if tier >= 1 {
                j["dry_run"] = json!(plan.unwrap_or("POST /v1/admin/changes/preview"));
            }
            if let Some(t) = tool {
                j["tool"] = json!(t);
            }
            if let Some(f) = experimental(r.path) {
                j["experimental"] = json!(f);
            }
            out.push(j);
        }
    }
    out
}

/// `GET /v1/catalog`'s answer.
pub fn answer(q: Option<&str>) -> Json {
    let routes = entries(q);
    json!({
        "routes": routes,
        "count": routes.len(),
        "errors": crate::errors::table(),
        "tiers": {
            "0": "reads: views, plans and dry runs",
            "1": "bounded and undone by itself (contain, cancel a query, silence or acknowledge an alert, a note): an ops key acts alone, with a reason",
            "2": "changes the cluster or its data and can be undone: an agent files a change request (POST /v1/admin/changes) for a person to approve",
            "3": "loses data or costs money: as tier 2, and an agent never does it alone through MCP or with an agent key",
        },
        "agents": "send x-chloedb-reason with every change (tier 1 and up), and x-chloedb-agent-model, -session and -run to be named in the audit log",
        "experimental": {
            "about": "a route marked `experimental` belongs to a feature with no compatibility promise yet: its API, settings and stored state may change in any release; its answers carry the x-chloedb-experimental header",
            "features": EXPERIMENTAL.iter().fold(serde_json::Map::new(), |mut m, (_, f, what)| {
                m.entry(f.to_string()).or_insert_with(|| json!(what));
                m
            }),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_routes_are_read_from_the_router() {
        let all = routes();
        assert!(all.len() > 180, "{}", all.len());
        let moves = all.iter().find(|r| r.path == "/v1/admin/moves").unwrap();
        assert!(moves.methods.contains(&Method::GET) && moves.methods.contains(&Method::POST), "{moves:?}");
        let contain = all.iter().find(|r| r.path == "/v1/admin/contain/:id").unwrap();
        assert_eq!(contain.methods, vec![Method::DELETE]);
        for (m, p, t) in TOOLS {
            assert!(all.iter().any(|r| r.path == *p && r.methods.iter().any(|x| x.as_str() == *m)), "{t}: no route {m} {p}");
        }
    }

    #[test]
    fn experimental_features_are_marked() {
        assert_eq!(experimental("/v1/admin/hub"), Some("hub"));
        assert_eq!(experimental("/v1/admin/hub/connections/x"), Some("hub"));
        assert_eq!(experimental("/c/:name/*rest"), Some("hub"));
        assert_eq!(experimental("/c/prod/v1/sql"), Some("hub"));
        assert_eq!(experimental("/classic"), None);
        assert_eq!(experimental("/v1/admin/hubs"), None);
        assert_eq!(experimental("/v1/admin/aws/plan?x=1"), Some("cloud"));
        assert_eq!(experimental("/v1/admin/kube/autoscale/dismiss"), Some("autoscale"));
        assert_eq!(experimental("/v1/admin/kube/plan"), None);
        assert_eq!(experimental("/v1/admin/oncall/runs/7"), Some("oncall"));
        assert_eq!(experimental("/v1/sql"), None);
        // Every prefix is a route the server has, so none is a stale mark.
        for (p, f, _) in EXPERIMENTAL {
            assert!(routes().iter().any(|r| experimental(r.path) == Some(f) && r.path.starts_with(p)), "{p}: no route");
        }
        let all = entries(None);
        let marked = all.iter().filter(|e| e["experimental"].is_string()).count();
        assert!(marked >= 20, "{marked}");
        assert!(all.iter().any(|e| e["path"] == "/v1/admin/oncall" && e["experimental"] == "oncall"));
        assert!(all.iter().all(|e| e["path"] != "/v1/sql" || e.get("experimental").is_none()));
        assert_eq!(answer(None)["experimental"]["features"].as_object().unwrap().len(), 4);
    }

    #[test]
    fn paths_match_their_routes() {
        assert_eq!(route_of("/v1/admin/shards/2/drain"), Some("/v1/admin/shards/:i/drain"));
        assert_eq!(route_of("/v1/admin/shards/2"), Some("/v1/admin/shards/:i"));
        assert_eq!(route_of("/v1/admin/plan/apply"), Some("/v1/admin/plan/apply"));
        assert_eq!(route_of("/v1/sql?x=1"), Some("/v1/sql"));
        assert_eq!(route_of("/v1/queries/abc"), Some("/v1/queries/:id"));
        assert_eq!(route_of("/v1/queries/slow"), Some("/v1/queries/slow"));
        assert_eq!(route_of("/v1/nothing/here"), None);
    }
}
