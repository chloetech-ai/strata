//! The command center: the page the server answers at `/`, and the
//! scripts and stylesheet it loads from `/ui/`, all compiled into the
//! binary (no CDN: it works offline and behind a firewall). The console it
//! grew from is at `/classic`.
//!
//! The page names its files under `/ui/v-<hash of them all>/`, which the
//! browser keeps for good: a reload asks for the page alone, and a new
//! build's page names new paths. Under `/ui/` itself, or another build's
//! tag, they are asked for again each time.
//!
//! With `STRATA_UI_DIR` set to a checkout's `strata-server/ui`, the server
//! reads the files from there on every request instead, so that an edit
//! shows on a reload without a rebuild; the page then names them under
//! `/ui/` alone.

use std::sync::OnceLock;

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};

pub const INDEX: &str = include_str!("../ui/index.html");
pub const CLASSIC: &str = include_str!("../ui/classic.html");

const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// Each file under `/ui/`: its path there, type and contents.
pub const FILES: &[(&str, &str, &str)] = &[
    ("app.css", CSS, include_str!("../ui/app/app.css")),
    ("core.js", JS, include_str!("../ui/app/core.js")),
    ("charts.js", JS, include_str!("../ui/app/charts.js")),
    ("data.js", JS, include_str!("../ui/app/data.js")),
    ("main.js", JS, include_str!("../ui/app/main.js")),
    ("sqledit.js", JS, include_str!("../ui/app/sqledit.js")),
    ("sqlguard.js", JS, include_str!("../ui/app/sqlguard.js")),
    ("pan.js", JS, include_str!("../ui/app/pan.js")),
    ("catalog.js", JS, include_str!("../ui/app/catalog.js")),
    ("paths.js", JS, include_str!("../ui/app/paths.js")),
    ("results.js", JS, include_str!("../ui/app/results.js")),
    ("profile.js", JS, include_str!("../ui/app/profile.js")),
    ("flame.js", JS, include_str!("../ui/app/flame.js")),
    ("workload.js", JS, include_str!("../ui/app/workload.js")),
    ("spec.js", JS, include_str!("../ui/app/spec.js")),
    ("follows.js", JS, include_str!("../ui/app/follows.js")),
    ("views/index.js", JS, include_str!("../ui/app/views/index.js")),
    ("views/overview.js", JS, include_str!("../ui/app/views/overview.js")),
    ("views/timeline.js", JS, include_str!("../ui/app/views/timeline.js")),
    ("views/cluster.js", JS, include_str!("../ui/app/views/cluster.js")),
    ("views/rollout.js", JS, include_str!("../ui/app/views/rollout.js")),
    ("views/autoscale.js", JS, include_str!("../ui/app/views/autoscale.js")),
    ("views/cloud.js", JS, include_str!("../ui/app/views/cloud.js")),
    ("views/connections.js", JS, include_str!("../ui/app/views/connections.js")),
    ("views/clusters.js", JS, include_str!("../ui/app/views/clusters.js")),
    ("views/follow.js", JS, include_str!("../ui/app/views/follow.js")),
    ("views/querypath.js", JS, include_str!("../ui/app/views/querypath.js")),
    ("views/performance.js", JS, include_str!("../ui/app/views/performance.js")),
    ("views/alerts.js", JS, include_str!("../ui/app/views/alerts.js")),
    ("views/queries.js", JS, include_str!("../ui/app/views/queries.js")),
    ("views/statement.js", JS, include_str!("../ui/app/views/statement.js")),
    ("views/profile.js", JS, include_str!("../ui/app/views/profile.js")),
    ("views/logs.js", JS, include_str!("../ui/app/views/logs.js")),
    ("views/access.js", JS, include_str!("../ui/app/views/access.js")),
    ("views/system.js", JS, include_str!("../ui/app/views/system.js")),
    ("views/sql.js", JS, include_str!("../ui/app/views/sql.js")),
    ("views/builder.js", JS, include_str!("../ui/app/views/builder.js")),
    ("views/schema.js", JS, include_str!("../ui/app/views/schema.js")),
    ("views/inspector.js", JS, include_str!("../ui/app/views/inspector.js")),
    ("views/tables.js", JS, include_str!("../ui/app/views/tables.js")),
    ("views/import.js", JS, include_str!("../ui/app/views/import.js")),
    ("views/relevance.js", JS, include_str!("../ui/app/views/relevance.js")),
    ("views/recsys.js", JS, include_str!("../ui/app/views/recsys.js")),
    ("views/sid.js", JS, include_str!("../ui/app/views/sid.js")),
    ("views/autocomplete.js", JS, include_str!("../ui/app/views/autocomplete.js")),
    ("views/media.js", JS, include_str!("../ui/app/views/media.js")),
    ("views/graph.js", JS, include_str!("../ui/app/views/graph.js")),
    ("views/classic.js", JS, include_str!("../ui/app/views/classic.js")),
];

/// A file from `STRATA_UI_DIR`, when it is set: `rel` under it.
fn from_disk(rel: &str) -> Option<String> {
    let dir = std::env::var_os("STRATA_UI_DIR")?;
    std::fs::read_to_string(std::path::Path::new(&dir).join(rel)).ok()
}

/// How long a browser keeps the files under this build's tag: for good.
const KEEP: &str = "public, max-age=31536000, immutable";

/// This build's tag for its files under `/ui/`: `v-` and the hash of them
/// all; none when they come from `STRATA_UI_DIR`, where they change.
pub fn tag() -> Option<&'static str> {
    static TAG: OnceLock<String> = OnceLock::new();
    if std::env::var_os("STRATA_UI_DIR").is_some() {
        return None;
    }
    Some(TAG.get_or_init(|| {
        let mut all = Vec::new();
        for (p, _, body) in FILES {
            all.extend_from_slice(p.as_bytes());
            all.push(0);
            all.extend_from_slice(body.as_bytes());
            all.push(0);
        }
        format!("v-{:016x}", twox_hash::XxHash3_64::oneshot(&all))
    }))
}

/// The page; the browser asks again each time (`no-cache`), so a new
/// binary's files, under their new tag, are seen at once.
pub async fn index() -> Response {
    let page = from_disk("index.html").unwrap_or_else(|| INDEX.into());
    let page = match tag() {
        Some(t) => page.replace("\"/ui/", &format!("\"/ui/{t}/")),
        None => page,
    };
    ([(header::CACHE_CONTROL, "no-cache")], Html(page)).into_response()
}

pub async fn classic() -> Response {
    ([(header::CACHE_CONTROL, "no-cache")], Html(from_disk("classic.html").unwrap_or_else(|| CLASSIC.into()))).into_response()
}

/// A file under `/ui/`, or under a build's tag there: kept for good under
/// this build's, asked for again each time otherwise (a page of another
/// build, during an upgrade, gets this build's file).
pub async fn file(Path(path): Path<String>) -> Response {
    let (path, cache) = match path.split_once('/') {
        Some((t, rest)) if t.starts_with("v-") => (rest, if tag() == Some(t) { KEEP } else { "no-cache" }),
        _ => (path.as_str(), "no-cache"),
    };
    match FILES.iter().find(|(p, ..)| *p == path) {
        Some((p, ty, body)) => {
            let body = from_disk(&format!("app/{p}")).unwrap_or_else(|| (*body).into());
            ([(header::CONTENT_TYPE, *ty), (header::CACHE_CONTROL, cache)], body).into_response()
        }
        None => (StatusCode::NOT_FOUND, "no such file in the command center").into_response(),
    }
}
