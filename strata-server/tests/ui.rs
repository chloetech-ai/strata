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

    let page = reqwest::get(format!("{base}/")).await.unwrap().text().await.unwrap();
    assert!(page.contains("/ui/main.js") && page.contains("/ui/app.css"), "{page}");

    // Every module reachable from main.js, and the stylesheet.
    let mut todo = vec!["main.js".to_string()];
    let mut seen = BTreeSet::new();
    while let Some(p) = todo.pop() {
        if !seen.insert(p.clone()) {
            continue;
        }
        let r = reqwest::get(format!("{base}/ui/{p}")).await.unwrap();
        assert_eq!(r.status(), 200, "/ui/{p}");
        assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"), "/ui/{p}");
        let js = r.text().await.unwrap();
        todo.extend(imports(&p, &js));
    }
    let css = reqwest::get(format!("{base}/ui/app.css")).await.unwrap();
    assert!(css.headers()["content-type"].to_str().unwrap().starts_with("text/css"));
    // Each served file is used, and each view is among them.
    let served: BTreeSet<String> = ui::FILES.iter().map(|(p, ..)| p.to_string()).filter(|p| p.ends_with(".js")).collect();
    assert_eq!(seen, served, "the modules main.js reaches, and those the server serves");
    for v in ["overview", "topology", "fleet", "querypath", "control", "rollout", "autoscale", "cloud", "cluster", "performance", "alerts", "queries", "logs", "sql", "builder", "schema", "import", "relevance", "recsys", "sid", "autocomplete", "media", "graph", "access", "system"] {
        assert!(seen.contains(&format!("views/{v}.js")), "{v}");
    }

    assert_eq!(reqwest::get(format!("{base}/ui/nope.js")).await.unwrap().status(), 404);
    assert_eq!(reqwest::get(format!("{base}/ui/../src/ui.rs")).await.unwrap().status(), 404);
    let classic = reqwest::get(format!("{base}/classic")).await.unwrap().text().await.unwrap();
    assert!(classic.contains("Strata query console"));
}
