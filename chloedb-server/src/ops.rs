//! Administration by agents: what each route risks (its tier), what the
//! keys for operations may call ([`Role`]), and how an agent is told
//! apart from a person.
//!
//! | tier | what | who may |
//! |---|---|---|
//! | 0 | reads: views, plans, dry runs | a key of the route's scope; a `propose` or `ops` key the server's views too |
//! | 1 | bounded and undone by itself: a contain rule (it expires), a query cancelled, an alert acknowledged or silenced (for a while), a note, a log filter (it expires), a profile, a scrub or verify | an `ops` key, with a reason |
//! | 2 | changes the cluster or its data, and can be undone: a plan applied, a spec, an upgrade, rank profiles, synonyms, alert rules, keys made or rotated, rows written | a key of the route's scope; an agent files a change request ([`crate::changes`]) |
//! | 3 | loses data or costs money: a shard removed, a promotion, a follow stopped, writes held, a key revoked, a tenant deleted, a cloud cluster made or deleted, vacuum | as tier 2, and an agent may not do it alone through MCP or with an agent key |
//!
//! A role key reads tables as a read key does and writes no row
//! ([`crate::keys::Role`]); what it may do on the server is decided here,
//! by route, and the service checks nothing more for it.
//!
//! An agent is a request made with a key made for one (`agent: true`,
//! or a role), through MCP (the `/v1/mcp` tools, the client's app `mcp`
//! or `mcp-stdio`), by the on-call agent (`oncall`), or one that says so
//! in `x-chloedb-agent-model`, `-session` or `-run`. An agent with a
//! person's admin key and none of these looks like a person: give agents
//! agent keys.

use axum::http::Method;
use serde_json::{json, Value as Json};

use crate::keys::{Caller, Principal, Role};

/// What a route risks ([`tier`]).
pub type Tier = u8;

/// Routes that read though they are sent by POST: plans, dry runs,
/// searches of the logs, replays.
pub const READ_POSTS: &[&str] = &[
    "/v1/admin/plan",
    "/v1/admin/kube/plan",
    "/v1/admin/simulator/predict",
    "/v1/logs/query",
    "/v1/admin/autoscale/replay",
    "/v1/admin/aws/validate",
    "/v1/admin/digitalocean/validate",
    "/v1/alerts/backtest",
    "/v1/query",
    "/v1/explain",
    "/v1/term_stats",
    "/v1/embed",
    "/v1/classify",
    "/v1/understand",
    "/v1/correct",
    "/v1/derive/preview",
    "/v1/schema/check",
    "/v1/kv/get",
    "/v1/snapshot",
    "/v1/rank_log/export",
    "/v1/rank_log/interleaving",
    "/v1/synonyms/mine",
    "/v1/mcp",
    "/v1/admin/changes/preview",
];

/// Routes that write rows (and the agents that spend Claude's tokens on
/// a table's behalf): changes of data, tier 2, never an operation.
const DATA: &[&str] = &["/v1/write", "/v1/import", "/v1/stream", "/v1/kv/:table/:key", "/v1/agent", "/v1/judge", "/v1/sql", "/v1/plan", "/v1/connectors/:name/push"];

/// Whether a request to `route` by `method` changes nothing.
pub fn is_read(method: &Method, route: &str) -> bool {
    *method == Method::GET || *method == Method::HEAD || READ_POSTS.contains(&route)
}

/// What a request to `route` by `method` risks: 0 to 3 (the module's
/// table). A statement sent to `/v1/sql` is judged by itself
/// ([`sql_tier`]).
pub fn tier(method: &Method, route: &str) -> Tier {
    if is_read(method, route) {
        return 0;
    }
    let m = method.as_str();
    match (m, route) {
        // 1: bounded, and undone by itself or by a person in a click.
        ("POST", "/v1/admin/contain") | ("DELETE", "/v1/admin/contain/:id") => 1,
        ("DELETE", "/v1/queries/:id") | ("DELETE", "/v1/queries") => 1,
        ("POST", "/v1/alerts/:rule/ack") | ("POST", "/v1/alerts/:rule/silence") | ("DELETE", "/v1/alerts/:rule/silence") | ("POST", "/v1/alerts/:rule/notes") => 1,
        ("POST", "/v1/events/notes") => 1,
        ("PUT", "/v1/logs/filter") | ("PUT", "/v1/profile/settings") | ("POST", "/v1/profile/cpu") | ("POST", "/v1/profile/heap") => 1,
        ("POST", "/v1/admin/checkpoint") | ("POST", "/v1/admin/scrub") | ("POST", "/v1/admin/verify") | ("POST", "/v1/admin/copies/scrub") | ("POST", "/v1/admin/copies/verify") | ("POST", "/v1/admin/copies/checkpoint") => 1,
        ("POST", "/v1/admin/oncall") | ("POST", "/v1/admin/oncall/runs/:id/stop") => 1,
        ("POST", "/v1/admin/simulator/stop") | ("POST", "/v1/admin/plan/run/stop") | ("POST", "/v1/admin/moves/stop") => 1,
        ("POST", "/v1/admin/kube/autoscale/dismiss") | ("POST", "/v1/queries/stats/reset") | ("POST", "/v1/index/reads/reset") => 1,
        ("POST", "/v1/feedback") | ("PATCH", "/v1/feedback/:id") => 1,
        ("POST", "/v1/admin/changes") | ("POST", "/v1/admin/changes/:id/withdraw") => 1,
        // 3: loses data, stops writes, or costs money.
        ("DELETE", "/v1/admin/shards/:i") | ("POST", "/v1/admin/promote") | ("POST", "/v1/admin/shards/:i/promote") => 3,
        ("POST", "/v1/admin/follow/stop") | ("POST", "/v1/admin/follow/promote") | ("POST", "/v1/admin/hold-writes") => 3,
        ("DELETE", "/v1/admin/keys/:id") | ("DELETE", "/v1/admin/tenants/:id") | ("POST", "/v1/admin/vacuum") => 3,
        ("POST", "/v1/admin/cloud/clusters") | ("POST", "/v1/admin/cloud/clusters/:id/delete") | ("POST", "/v1/admin/cloud/found/delete") => 3,
        _ => 2,
    }
}

/// Whether `route` writes rows rather than changing the server.
pub fn is_data(route: &str) -> bool {
    DATA.contains(&route)
}

/// Routes no role key calls, whatever their tier: they copy every row
/// out, carry other clusters' or clouds' credentials, or run SQL in a
/// tenant.
const NOT_FOR_ROLES: &[&str] = &[
    "/v1/export",
    "/c/:name/*rest",
    "/v1/admin/tenants/:id/sql",
    "/v1/admin/tenants/:id/mcp",
    "/v1/admin/tenants/:id/board/:tool",
    "/v1/admin/tenants/:id/base",
    "/v1/admin/aws/credentials",
    "/v1/admin/digitalocean/token",
    "/v1/admin/hub/copy",
];

/// Routes whose body or answer holds a credential: those of
/// [`NOT_FOR_ROLES`], and those that make or show a key's secret, or take
/// another cluster's, a connector's or a cloud's. A change request does
/// not carry them (audit X-D1): it is kept on disk and read by every role
/// key, and so would be what it carries and what it was answered.
const CREDENTIAL_ROUTES: &[&str] = &[
    "/v1/admin/keys",
    "/v1/admin/keys/:id/rotate",
    "/v1/keys/own",
    "/v1/admin/connectors",
    "/v1/admin/connectors/test",
    "/v1/admin/connectors/:name",
    "/v1/admin/connectors/:name/key",
    "/v1/connectors/:name/push",
    "/v1/admin/hub/connections",
    "/v1/admin/hub/connections/:name",
    "/v1/admin/hub/adopt",
    "/v1/admin/aws/validate",
    "/v1/admin/aws/plan",
    "/v1/admin/digitalocean/validate",
    "/v1/admin/digitalocean/plan",
];

/// Whether `route` carries or answers a credential ([`CREDENTIAL_ROUTES`]).
pub fn carries_credentials(route: &str) -> bool {
    NOT_FOR_ROLES.contains(&route) || CREDENTIAL_ROUTES.contains(&route)
}

/// Whether a key with role `role` may call `route` by `method` beyond
/// what its scope (read) allows: the server's views (tier 0) for both
/// roles, and for `ops` the bounded actions (tier 1).
pub fn role_may(role: Option<Role>, method: &Method, route: &str) -> bool {
    let Some(role) = role else { return false };
    if NOT_FOR_ROLES.contains(&route) || is_data(route) {
        return false;
    }
    if route.starts_with("/v1/apps/") && !is_read(method, route) {
        return false;
    }
    match tier(method, route) {
        0 => true,
        // Filing a change request is a proposer's whole job.
        1 if route.starts_with("/v1/admin/changes") => true,
        1 => role >= Role::Ops,
        _ => false,
    }
}

/// The apps (`x-chloedb-app`) agents' requests come by.
const AGENT_APPS: &[&str] = &["mcp", "mcp-stdio", "oncall"];

/// What a request says of the agent that sent it, if it says anything:
/// `{model?, session?, run?}` from `x-chloedb-agent-*`.
pub fn agent_of(caller: &Caller) -> Option<Json> {
    let mut j = serde_json::Map::new();
    for (k, v) in [("model", &caller.agent_model), ("session", &caller.agent_session), ("run", &caller.agent_run)] {
        if let Some(v) = v {
            j.insert(k.into(), json!(v));
        }
    }
    (!j.is_empty()).then_some(Json::Object(j))
}

/// Whether a request is an agent's (the module's last paragraph).
pub fn is_agent(who: &Principal, caller: Option<&Caller>) -> bool {
    who.is_agent() || who.role.is_some() || caller.is_some_and(|c| c.app.as_deref().is_some_and(|a| AGENT_APPS.contains(&a)) || agent_of(c).is_some())
}

/// The most a tier 1 action of an agent may last: a contain rule.
pub const AGENT_CONTAIN_S: u64 = 3600;
/// The most an agent may silence an alert for.
pub const AGENT_SILENCE_MIN: u64 = 120;

/// What a key may do, for `GET /v1/whoami` (`may`): the tiers it may
/// act at alone, whether it may file change requests, and whether it
/// may approve them.
pub fn may(who: &Principal) -> Json {
    use crate::keys::Scope;
    let admin = who.scope >= Scope::Admin && !who.limited();
    let (views, acts): (bool, Tier) = match (admin, who.role) {
        (true, _) => (true, 3),
        (false, Some(Role::Ops)) => (true, 1),
        (false, Some(Role::Propose)) => (true, 0),
        (false, None) => (false, 0),
    };
    let data = match (who.role, who.scope) {
        (Some(_), _) => "read",
        (None, s) => s.name(),
    };
    json!({
        "data": data,
        "server_views": views,
        "act_alone_up_to_tier": if views { json!(acts) } else { Json::Null },
        "propose_changes": views,
        "approve_changes": admin && !who.is_agent(),
        "agent": who.is_agent(),
        "reason_required": who.is_agent(),
        "tables": who.tables.as_deref().map(crate::keys::grants_json),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_of_some_routes() {
        assert_eq!(tier(&Method::GET, "/v1/admin/audit"), 0);
        assert_eq!(tier(&Method::POST, "/v1/admin/plan"), 0);
        assert_eq!(tier(&Method::POST, "/v1/admin/contain"), 1);
        assert_eq!(tier(&Method::DELETE, "/v1/queries/:id"), 1);
        assert_eq!(tier(&Method::POST, "/v1/admin/plan/apply"), 2);
        assert_eq!(tier(&Method::POST, "/v1/admin/kube/upgrade"), 2);
        assert_eq!(tier(&Method::DELETE, "/v1/admin/shards/:i"), 3);
        assert_eq!(tier(&Method::POST, "/v1/admin/cloud/clusters"), 3);
        assert_eq!(tier(&Method::POST, "/v1/write"), 2);
    }

    #[test]
    fn roles_see_views_ops_act_alone_on_tier_one_only() {
        let (ops, propose) = (Some(Role::Ops), Some(Role::Propose));
        assert!(role_may(propose, &Method::GET, "/v1/admin/audit"));
        assert!(role_may(propose, &Method::POST, "/v1/admin/plan"));
        assert!(role_may(propose, &Method::POST, "/v1/admin/changes"));
        assert!(!role_may(propose, &Method::POST, "/v1/admin/contain"));
        assert!(role_may(ops, &Method::POST, "/v1/admin/contain"));
        assert!(role_may(ops, &Method::DELETE, "/v1/queries/:id"));
        assert!(!role_may(ops, &Method::POST, "/v1/admin/plan/apply"));
        assert!(!role_may(ops, &Method::DELETE, "/v1/admin/shards/:i"));
        assert!(!role_may(ops, &Method::POST, "/v1/write"));
        assert!(!role_may(ops, &Method::GET, "/v1/export"));
        assert!(!role_may(ops, &Method::GET, "/c/:name/*rest"));
        assert!(!role_may(None, &Method::GET, "/v1/admin/audit"));
    }
}
