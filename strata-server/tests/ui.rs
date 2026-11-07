//! The command center is served whole: the page, every module it loads
//! (followed through their imports, so a file missing from the server's
//! table fails here), the classic console, and nothing else under `/ui/`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::service::{AgentConfig, Backend, Service};
use strata_server::{demo, server, ui};
use tokio::net::TcpListener;

/// The modules a script imports, as paths under `/ui/` from its own.
fn imports(path: &str, js: &str) -> Vec<String> {
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    let mut out = Vec::new();
    for quote in ["from \"", "import(\""] {
        for (i, _) in js.match_indices(quote) {
            let rest = &js[i + quote.len()..];
            let Some(end) = rest.find('"') else { continue };
            let spec = &rest[..end];
            if !spec.starts_with('.') {
                continue;
            }
            let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
            for seg in spec.split('/') {
                match seg {
                    "." => {}
                    ".." => {
                        parts.pop();
                    }
                    s => parts.push(s),
                }
            }
            out.push(parts.join("/"));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn the_command_center_is_served_with_every_module_it_imports() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("ui.strata"), demo::schema(10), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let svc = Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent, 4, Duration::from_secs(5)));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(svc, l, Default::default()));

    let r = strata_server::http_client().build().unwrap().get(format!("{base}/")).send().await.unwrap();
    assert_eq!(r.headers()["cache-control"], "no-cache");
    let page = r.text().await.unwrap();
    // The page names its files under this build's tag, which the browser
    // keeps for good (a reload asks for the page alone).
    let at = match ui::tag() {
        Some(t) => format!("/ui/{t}/"),
        None => "/ui/".to_string(),
    };
    assert!(page.contains(&format!("{at}main.js")) && page.contains(&format!("{at}app.css")), "{page}");
    let keep = |r: &reqwest::Response| r.headers()["cache-control"].to_str().unwrap().to_string();
    if ui::tag().is_some() {
        let r = strata_server::http_client().build().unwrap().get(format!("{base}{at}views/sql.js")).send().await.unwrap();
        assert_eq!((r.status().as_u16(), keep(&r).as_str()), (200, "public, max-age=31536000, immutable"));
        // Untagged, or another build's tag (a page from before an upgrade):
        // this build's file, asked for again each time.
        for other in ["/ui/views/sql.js", "/ui/v-0123456789abcdef/views/sql.js"] {
            let r = strata_server::http_client().build().unwrap().get(format!("{base}{other}")).send().await.unwrap();
            assert_eq!((r.status().as_u16(), keep(&r).as_str()), (200, "no-cache"), "{other}");
        }
    }

    // Every module reachable from main.js, and the stylesheet.
    let mut todo = vec!["main.js".to_string()];
    let mut seen = BTreeSet::new();
    while let Some(p) = todo.pop() {
        if !seen.insert(p.clone()) {
            continue;
        }
        let r = strata_server::http_client().build().unwrap().get(format!("{base}{at}{p}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{at}{p}");
        assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"), "{at}{p}");
        let js = r.text().await.unwrap();
        todo.extend(imports(&p, &js));
    }
    let css = strata_server::http_client().build().unwrap().get(format!("{base}{at}app.css")).send().await.unwrap();
    assert!(css.headers()["content-type"].to_str().unwrap().starts_with("text/css"));
    // Each served file is used, and each view is among them.
    let served: BTreeSet<String> = ui::FILES.iter().map(|(p, ..)| p.to_string()).filter(|p| p.ends_with(".js")).collect();
    assert_eq!(seen, served, "the modules main.js reaches, and those the server serves");
    for v in ["overview", "timeline", "querypath", "tables", "statement", "rollout", "autoscale", "cloud", "cluster", "performance", "alerts", "queries", "logs", "sql", "builder", "schema", "inspector", "import", "relevance", "recsys", "sid", "autocomplete", "media", "graph", "access", "system"] {
        assert!(seen.contains(&format!("views/{v}.js")), "{v}");
    }

    assert_eq!(strata_server::http_client().build().unwrap().get(format!("{base}/ui/nope.js")).send().await.unwrap().status(), 404);
    assert_eq!(strata_server::http_client().build().unwrap().get(format!("{base}/ui/../src/ui.rs")).send().await.unwrap().status(), 404);
    let classic = strata_server::http_client().build().unwrap().get(format!("{base}/classic")).send().await.unwrap().text().await.unwrap();
    assert!(classic.contains("Strata query console"));

    // SH-11: gzip for a client that asks, of JSON and the UI's files; not
    // for one that does not, nor for event streams.
    let c = strata_server::http_client().build().unwrap();
    let gz = |path: &'static str| {
        let c = c.clone();
        let base = base.clone();
        async move { c.get(format!("{base}{path}")).header("accept-encoding", "gzip").send().await.unwrap() }
    };
    for path in ["/ui/main.js", "/v1/schema", "/v1/alerts"] {
        let r = gz(path).await;
        assert_eq!((r.status().as_u16(), r.headers().get("content-encoding").map(|v| v.to_str().unwrap().to_string())), (200, Some("gzip".into())), "{path}");
    }
    let plain = c.get(format!("{base}/ui/main.js")).send().await.unwrap();
    assert!(plain.headers().get("content-encoding").is_none() && plain.text().await.unwrap().contains("import"));
}
