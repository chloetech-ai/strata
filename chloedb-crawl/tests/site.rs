//! The crawler on a site made for it (a shop: lists of products with
//! JSON-LD, spec tables and a template field, a robots.txt that keeps it
//! out of a path, and a sitemap), served here:
//!
//! - a preview reads the site and plans its table, with no Strata;
//! - the API refuses browsers and requests without its token;
//! - a crawl into a local cluster of real strata-servers makes the table
//!   from the first pages, adds a column a later field needs, writes every
//!   page once, and grows the cluster past one shard as the table fills.
//!   It needs the chloedb-server binary (`CHLOEDB_SERVER_BIN`, else
//!   `../chloedb-server/target/{ci,release,debug}/chloedb-server`), and is
//!   skipped without it unless `CHLOEDB_CRAWL_NEED_SERVER` is set.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use chloedb_crawl::api::{self, Service};
use chloedb_crawl::grow::{Grower, How, Local, Policy};
use chloedb_crawl::job::Spec;
use chloedb_crawl::strata::Strata;

const PRODUCTS: usize = 150;
const PER_LIST: usize = 10;
/// Products from this one on have a warranty: a field the first pages
/// lack, so its column comes during the crawl.
const WARRANTY_FROM: usize = 40;

#[derive(Clone, Default)]
struct Site {
    base: Arc<Mutex<String>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl Site {
    fn hit(&self, path: &str) {
        *self.hits.lock().unwrap().entry(path.to_string()).or_default() += 1;
    }
    fn hits(&self, prefix: &str) -> usize {
        self.hits.lock().unwrap().iter().filter(|(p, _)| p.starts_with(prefix)).map(|(_, n)| n).sum()
    }
}

fn html(body: String) -> axum::response::Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

const COLORS: &[&str] = &["red", "green", "blue", "black", "white"];
const WORDS: &[&str] = &["sturdy", "light", "quiet", "bright", "compact", "classic", "handmade", "folding", "waterproof", "vintage"];

fn product(i: usize) -> String {
    let warranty = if i >= WARRANTY_FROM { format!("<tr><th>Warranty</th><td>{} years</td></tr>", 1 + i % 3) } else { String::new() };
    let w = |k: usize| WORDS[(i * 7 + k) % WORDS.len()];
    format!(
        r#"<!doctype html><html lang="en"><head><title>Widget {i} | Shop</title>
<meta name="description" content="Widget {i}, a {a} widget for every day">
<meta property="og:type" content="product">
<script type="application/ld+json">{{"@context":"https://schema.org","@type":"Product","name":"Widget {i}","sku":"W-{i:04}","offers":{{"@type":"Offer","price":"{price}.00","priceCurrency":"USD"}}}}</script>
</head><body><nav><a href="/">Home</a> <a href="/list/1">Shop</a> <a href="/about">About</a></nav>
<main><h1>Widget {i}</h1><p class="stock-level">{left} left</p>
<table><tr><th>Weight</th><td>{kg} kg</td></tr><tr><th>Color</th><td>{color}</td></tr>{warranty}</table>
<p>Widget {i} is {a} and {b}: a {c} thing to keep at home, at work, or on the road. People who bought it say it is {b} and {c}.</p></main>
<footer>Shop, since 2001</footer></body></html>"#,
        price = 10 + i,
        left = (i * 13) % 40 + 1,
        kg = 1 + i % 9,
        color = COLORS[i % COLORS.len()],
        a = w(0),
        b = w(3),
        c = w(5),
    )
}

fn list(n: usize) -> String {
    let lists = PRODUCTS.div_ceil(PER_LIST);
    let items: String = ((n - 1) * PER_LIST + 1..=(n * PER_LIST).min(PRODUCTS)).map(|i| format!(r#"<li><a href="/p/{i}">Widget {i}</a></li>"#)).collect();
    let next = if n < lists { format!(r#"<a href="/list/{}">next</a>"#, n + 1) } else { String::new() };
    format!(r#"<!doctype html><html lang="en"><head><title>Shop, page {n}</title></head><body><nav><a href="/">Home</a> <a href="/about">About</a></nav><main><h1>Widgets, page {n}</h1><ul>{items}</ul>{next}</main></body></html>"#)
}

async fn serve_site() -> (Site, String) {
    let site = Site::default();
    let app = Router::new()
        .route(
            "/",
            get(|State(s): State<Site>| async move {
                s.hit("/");
                html(r#"<!doctype html><html lang="en"><head><title>Shop</title><meta name="description" content="Widgets of every kind"></head><body><h1>Shop</h1>
<a href="/list/1">All widgets</a> <a href="/about">About</a> <a href="/private/secret">Staff</a> <a href="https://other.example/">Elsewhere</a> <a href="/logo.png">logo</a></body></html>"#.into())
            }),
        )
        .route("/about", get(|State(s): State<Site>| async move { s.hit("/about"); html("<html><head><title>About the shop</title></head><body><main><p>We sell widgets.</p></main></body></html>".into()) }))
        .route("/list/:n", get(|State(s): State<Site>, Path(n): Path<usize>| async move { s.hit(&format!("/list/{n}")); html(list(n)) }))
        .route("/p/:i", get(|State(s): State<Site>, Path(i): Path<usize>| async move { s.hit(&format!("/p/{i}")); html(product(i)) }))
        .route("/private/*rest", get(|State(s): State<Site>| async move { s.hit("/private/"); html("<html><body>staff only</body></html>".into()) }))
        .route(
            "/robots.txt",
            get(|State(s): State<Site>| async move {
                s.hit("/robots.txt");
                let base = s.base.lock().unwrap().clone();
                format!("User-agent: *\nDisallow: /private/\n\nSitemap: {base}/sitemap.xml\n")
            }),
        )
        .route(
            "/sitemap.xml",
            get(|State(s): State<Site>| async move {
                s.hit("/sitemap.xml");
                let base = s.base.lock().unwrap().clone();
                let urls: String = [PRODUCTS, PRODUCTS - 1].iter().map(|i| format!("<url><loc>{base}/p/{i}</loc></url>")).collect();
                ([(header::CONTENT_TYPE, "application/xml")], format!(r#"<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{urls}</urlset>"#))
            }),
        )
        .with_state(site.clone());
    // The sites served here are on this machine: reachable only when said.
    chloedb_crawl::reach::set(chloedb_crawl::reach::Reach::any());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    *site.base.lock().unwrap() = base.clone();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (site, base)
}

fn spec(v: Value) -> Spec {
    serde_json::from_value(v).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_plans_the_table_from_a_few_pages() {
    let (site, base) = serve_site().await;
    let p = chloedb_crawl::job::preview(&spec(json!({"seeds": [format!("{base}/list/1")], "table": "widgets"})), 8).await.unwrap();
    assert_eq!(p["table"], "widgets");
    assert_eq!(p["pages"].as_array().unwrap().len(), 8, "{p:#}");
    let cols: Vec<(String, String, String)> = p["plan"]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["name"].as_str().unwrap().to_string(), c["ty"].as_str().unwrap().to_string(), c["source"]["path"].as_str().unwrap_or("").to_string()))
        .collect();
    let col = |name: &str| cols.iter().find(|c| c.0 == name).unwrap_or_else(|| panic!("no column {name}: {cols:?}"));
    // JSON-LD's offer, the spec table's rows, the site's template.
    assert_eq!(col("offers_price"), &("offers_price".into(), "float".into(), "ld.offers.price".into()));
    assert_eq!(col("sku").1, "keyword");
    assert_eq!(col("color").2, "specs.color");
    assert!(p["template"].as_array().unwrap().iter().any(|t| t["selector"] == "p.stock-level"), "{}", p["template"]);
    assert!(p["sql"].as_str().unwrap().starts_with("CREATE TABLE IF NOT EXISTS widgets ("));
    assert!(p["sql"].as_str().unwrap().contains("data JSON"));
    // Read through robots.txt; the staff pages are not linked from here.
    assert_eq!(site.hits("/robots.txt"), 1);
    assert_eq!(site.hits("/private/"), 0);
}

/// From a shop's home, which links only to its first list, the preview
/// reads the list's products too, so it shows the fields a crawl will
/// extract (DAP-14: it read the home and the list, and showed none).
#[tokio::test(flavor = "multi_thread")]
async fn a_preview_from_the_home_page_reaches_the_products() {
    let (_site, base) = serve_site().await;
    let p = chloedb_crawl::job::preview(&spec(json!({"seeds": [format!("{base}/")], "table": "widgets"})), 8).await.unwrap();
    let urls: Vec<&str> = p["pages"].as_array().unwrap().iter().map(|x| x["url"].as_str().unwrap()).collect();
    assert!(urls.iter().filter(|u| u.contains("/p/")).count() >= 4, "{urls:?}");
    let cols: Vec<&str> = p["plan"]["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(cols.contains(&"offers_price") && cols.contains(&"sku"), "{cols:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_api_takes_no_browsers_and_needs_its_token() {
    let strata = Strata::new("http://127.0.0.1:9", None);
    let grower = Grower::new(Policy { how: How::Off, split_rows: 0, max_shards: 1 }, strata.clone(), None);
    let svc = Service::new(strata, grower, Some("crawl-secret".into()), false);
    // The sites served here are on this machine: reachable only when said.
    chloedb_crawl::reach::set(chloedb_crawl::reach::Reach::any());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, api::router(svc)).await.unwrap() });
    let c = reqwest::Client::new();
    let status = |r: reqwest::Response| r.status().as_u16();
    assert_eq!(status(c.get(format!("{base}/v1/crawls")).send().await.unwrap()), 401);
    assert_eq!(status(c.get(format!("{base}/v1/crawls")).bearer_auth("wrong").send().await.unwrap()), 401);
    assert_eq!(status(c.get(format!("{base}/v1/crawls")).bearer_auth("crawl-secret").header("origin", "https://evil.example").send().await.unwrap()), 403);
    let r = c.get(format!("{base}/v1/crawls")).bearer_auth("crawl-secret").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"crawls": []}));
    let m: Value = c.get(format!("{base}/app.json")).bearer_auth("crawl-secret").send().await.unwrap().json().await.unwrap();
    assert_eq!((m["name"].as_str(), m["view"].as_str()), (Some("crawler"), Some("ui/view.js")));
    let view = c.get(format!("{base}/ui/view.js")).bearer_auth("crawl-secret").send().await.unwrap().text().await.unwrap();
    assert!(view.contains("export default"));
    // A crawl that cannot start says why.
    let r = c.post(format!("{base}/v1/crawls")).bearer_auth("crawl-secret").json(&json!({"seeds": ["ftp://x"]})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let r = c.post(format!("{base}/v1/crawls")).bearer_auth("crawl-secret").json(&json!({"seeds": ["https://x.example/"], "rules": ["price=p.price["]})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400, "{}", r.text().await.unwrap());
}

fn server_bin() -> Option<PathBuf> {
    if let Some(b) = std::env::var_os("CHLOEDB_SERVER_BIN").or_else(|| std::env::var_os("STRATA_SERVER_BIN")) {
        return Some(PathBuf::from(b));
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../chloedb-server/target");
    ["ci", "release", "debug"].iter().map(|p| root.join(p).join("chloedb-server")).find(|p| p.exists())
}

/// Ports `base + 10 i` and `base + 10 i + 1` free for `n` shards.
fn free_base(n: u16) -> u16 {
    let seed = std::process::id() as u64 ^ std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    for k in 0..200u64 {
        let base = 20000 + ((seed.wrapping_mul(6364136223846793005).wrapping_add(k * 1442695040888963407) >> 33) % 35000) as u16;
        let base = base - base % 10;
        if (0..n).all(|i| [0, 1].iter().all(|d| std::net::TcpListener::bind(("127.0.0.1", base + 10 * i + d)).is_ok())) {
            return base;
        }
    }
    panic!("no free ports");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crawl_fills_a_table_and_grows_the_cluster() {
    let Some(bin) = server_bin() else {
        assert!(std::env::var_os("CHLOEDB_CRAWL_NEED_SERVER").is_none(), "no chloedb-server binary (CHLOEDB_SERVER_BIN)");
        eprintln!("skipped: no chloedb-server binary (build chloedb-server, or set CHLOEDB_SERVER_BIN)");
        return;
    };
    let (site, base) = serve_site().await;
    let dir = tempfile::tempdir().unwrap();
    let coordinator = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let local = Local::new(dir.path().join("cluster"), bin, coordinator, free_base(3), None);
    let url = local.start().await.unwrap();
    let strata = Strata::new(&url, None);
    let grower = Grower::new(Policy { how: How::Local, split_rows: 60, max_shards: 3 }, strata.clone(), Some(local.clone()));
    tokio::spawn(grower.clone().run());
    let svc = Service::new(strata.clone(), grower.clone(), None, true);

    let result = tokio::time::timeout(Duration::from_secs(240), async {
        let job = svc.start(spec(json!({"seeds": [format!("{base}/")], "table": "widgets", "max_depth": 40, "per_host": 4, "delay_ms": 0, "concurrency": 8, "sample": 30, "batch": 20}))).unwrap();
        while job.active() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let s = job.status();
        assert_eq!(s["phase"], "done", "{:#}", s["error"]);
        // Every page: the home page, About, the lists and the products
        // (some from the sitemap first); not the staff pages, nor the logo.
        let pages = 2 + PRODUCTS.div_ceil(PER_LIST) + PRODUCTS;
        assert_eq!(s["counts"]["written"], pages as u64, "{:#}", s["counts"]);
        assert_eq!(site.hits("/private/"), 0);
        assert_eq!(site.hits("/p/"), PRODUCTS, "a page fetched twice");
        assert!(site.hits("/sitemap.xml") >= 1);

        // The table: made from the first 30 pages, the warranty added later.
        let events = s["events"].as_array().unwrap();
        assert!(events.iter().any(|e| e["kind"] == "schema" && e["sql"].as_str().is_some_and(|q| q.starts_with("CREATE TABLE"))), "{events:#?}");
        let added: Vec<&str> = events.iter().filter(|e| e["kind"] == "column").filter_map(|e| e["sql"].as_str()).collect();
        assert!(added.iter().any(|q| q.contains("ADD COLUMN IF NOT EXISTS warranty")), "{events:#?}");
        let fields: Vec<String> = strata.table("widgets").await.unwrap().unwrap().iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
        for f in ["url", "title", "body", "offers_price", "sku", "color", "weight", "warranty", "stock_level", "data"] {
            assert!(fields.iter().any(|x| x == f), "no field {f}: {fields:?}");
        }
        (job, pages)
    })
    .await;
    let (job, pages) = match result {
        Ok(r) => r,
        Err(_) => {
            local.stop().await;
            panic!("the crawl did not end in 4 minutes: {:#}", svc.job("c1").map(|j| j.status()).unwrap_or_default());
        }
    };
    let _ = job;

    // The rows, through the coordinator: each page once, typed.
    let n = strata.query("SELECT COUNT(*) AS n FROM widgets", json!({})).await.unwrap();
    assert_eq!(n["rows"][0]["n"], pages as u64, "{n}");
    let n = strata.query("SELECT COUNT(*) AS n FROM widgets WHERE offers_price >= 100", json!({})).await.unwrap();
    assert_eq!(n["rows"][0]["n"], (PRODUCTS - 89) as u64, "{n}");
    let r = strata.query("SELECT sku, color, warranty FROM widgets WHERE url = :u", json!({"u": format!("{base}/p/42")})).await.unwrap();
    assert_eq!(r["rows"][0], json!({"sku": "W-0042", "color": "blue", "warranty": "1 years"}), "{r}");
    let r = strata.query("SELECT url FROM widgets WHERE MATCH(body, 'waterproof') LIMIT 500", json!({})).await.unwrap();
    assert!(!r["rows"].as_array().unwrap().is_empty());

    // The cluster grew past one shard as the table filled (60 rows a
    // shard, at most 3), and holds every row once the slots have moved.
    let t0 = Instant::now();
    let g = loop {
        let g = grower.status();
        let rows: u64 = g["shards"].as_array().map(|a| a.iter().map(|s| s["rows"].as_u64().unwrap_or(0)).sum()).unwrap_or(0);
        if (g["shards"].as_array().map_or(0, |a| a.len()) >= 2 && g["moving"] == false && rows == pages as u64) || t0.elapsed() > Duration::from_secs(60) {
            break g;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    local.stop().await;
    let shards = g["shards"].as_array().unwrap();
    assert!(shards.len() >= 2, "{g:#}");
    assert!(g["events"].as_array().unwrap().iter().any(|e| e["kind"] == "shard"), "{g:#}");
    assert_eq!(shards.iter().map(|s| s["rows"].as_u64().unwrap()).sum::<u64>(), pages as u64, "{g:#}");
    assert!(shards.iter().all(|s| s["rows"].as_u64().unwrap() > 0), "{g:#}");
}

/// `--grow servers=...`: empty servers started for the cluster, added one
/// at a time as shards fill, and no more once they are all in.
#[tokio::test(flavor = "multi_thread")]
async fn a_crawl_grows_onto_servers_it_was_given() {
    let Some(bin) = server_bin() else {
        assert!(std::env::var_os("CHLOEDB_CRAWL_NEED_SERVER").is_none(), "no chloedb-server binary (CHLOEDB_SERVER_BIN)");
        eprintln!("skipped: no chloedb-server binary (build chloedb-server, or set CHLOEDB_SERVER_BIN)");
        return;
    };
    let (_site, base) = serve_site().await;
    let dir = tempfile::tempdir().unwrap();
    let coordinator = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let ports = free_base(3);
    let local = Local::new(dir.path().join("cluster"), bin.clone(), coordinator, ports, None);
    let url = local.start().await.unwrap();
    // A spare server, not in the cluster: the one growing may add.
    let (http, grpc) = (format!("127.0.0.1:{}", ports + 20), format!("127.0.0.1:{}", ports + 21));
    let _spare = tokio::process::Command::new(&bin)
        .args(["--db", &dir.path().join("spare.strata").to_string_lossy(), "--seq-no", "--listen", &http, "--grpc", &grpc])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    while Strata::new(&format!("http://{http}"), None).health().await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the spare server did not start");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let strata = Strata::new(&url, None);
    let grower = Grower::new(Policy { how: How::Servers { servers: vec![format!("http://{grpc}")] }, split_rows: 30, max_shards: 4 }, strata.clone(), None);
    tokio::spawn(grower.clone().run());
    let svc = Service::new(strata.clone(), grower.clone(), None, false);
    let job = svc.start(spec(json!({"seeds": [format!("{base}/")], "table": "widgets", "max_depth": 40, "max_pages": 90, "per_host": 4, "delay_ms": 20, "batch": 10}))).unwrap();
    let t0 = Instant::now();
    while job.active() || grower.status()["moving"] == true || grower.status()["shards"].as_array().map_or(0, |a| a.len()) < 2 {
        if t0.elapsed() > Duration::from_secs(120) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // Past 30 rows a shard, with one server to add: two shards, then it
    // waits (once the rebalance's quiet time is over and the shards have
    // been past it on two looks).
    let t0 = Instant::now();
    let g = loop {
        let g = grower.status();
        if g["waiting"].as_str().is_some_and(|w| w.contains("every server")) || t0.elapsed() > Duration::from_secs(30) {
            break g;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    local.stop().await;
    // 90 pages handed out, the staff page among them (refused by robots.txt).
    let c = &job.status()["counts"];
    assert_eq!(c["written"].as_u64().unwrap() + c["misses"]["robots"].as_u64().unwrap(), 90, "{c:#}");
    let shards = g["shards"].as_array().unwrap();
    assert_eq!(shards.len(), 2, "{g:#}");
    assert_eq!(g["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "shard").count(), 1, "{g:#}");
    assert!(g["waiting"].as_str().is_some_and(|w| w.contains("every server")), "{g:#}");
}

/// Politely, into a cluster: a page that answers 429 (Retry-After: 1)
/// twice is asked again and written, the host waiting each time; and a
/// site the crawl reaches through a link, not a seed, gets the
/// Crawl-delay its robots.txt asks for although the crawl asks none.
#[tokio::test(flavor = "multi_thread")]
async fn a_crawl_waits_for_busy_pages_and_every_sites_crawl_delay() {
    let Some(bin) = server_bin() else {
        assert!(std::env::var_os("CHLOEDB_CRAWL_NEED_SERVER").is_none(), "no chloedb-server binary (CHLOEDB_SERVER_BIN)");
        eprintln!("skipped: no chloedb-server binary (build chloedb-server, or set CHLOEDB_SERVER_BIN)");
        return;
    };
    // The other site, linked as localhost: its pages a link apart, the time of each hit kept.
    let times: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let other = {
        let t = times.clone();
        let app = Router::new()
            .route("/robots.txt", get(|| async { "User-agent: *\nCrawl-delay: 0.4\n" }))
            .route(
                "/b/:i",
                get(move |Path(i): Path<usize>| {
                    let t = t.clone();
                    async move {
                        t.lock().unwrap().push(Instant::now());
                        let next = if i < 4 { format!(r#"<a href="/b/{}">next</a>"#, i + 1) } else { String::new() };
                        html(format!("<html><head><title>Other {i}</title></head><body><main><p>Another site, page {i}.</p>{next}</main></body></html>"))
                    }
                }),
            );
        // The sites served here are on this machine: reachable only when said.
        chloedb_crawl::reach::set(chloedb_crawl::reach::Reach::any());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://localhost:{port}")
    };
    let site = Site::default();
    let home = format!(r#"<html><head><title>Home</title></head><body><main><a href="/flaky">Flaky</a> <a href="/a/1">One</a> <a href="/a/2">Two</a> <a href="{other}/b/1">Elsewhere</a></main></body></html>"#);
    let app = Router::new()
        .route("/", get(move |State(s): State<Site>| { let home = home.clone(); async move { s.hit("/"); html(home) } }))
        .route("/a/:i", get(|State(s): State<Site>, Path(i): Path<usize>| async move { s.hit("/a/"); html(format!("<html><head><title>A {i}</title></head><body><main><p>Page {i}.</p></main></body></html>")) }))
        .route(
            "/flaky",
            get(|State(s): State<Site>| async move {
                s.hit("/flaky");
                if s.hits("/flaky") <= 2 {
                    (axum::http::StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "1")], "slow down").into_response()
                } else {
                    html("<html><head><title>Flaky</title></head><body><main><p>Here at last.</p></main></body></html>".into())
                }
            }),
        )
        .with_state(site.clone());
    // The sites served here are on this machine: reachable only when said.
    chloedb_crawl::reach::set(chloedb_crawl::reach::Reach::any());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let coordinator = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let local = Local::new(dir.path().join("cluster"), bin, coordinator, free_base(1), None);
    let url = local.start().await.unwrap();
    let strata = Strata::new(&url, None);
    let grower = Grower::new(Policy { how: How::Off, split_rows: 0, max_shards: 1 }, strata.clone(), None);
    let svc = Service::new(strata.clone(), grower, None, true);
    let job = svc.start(spec(json!({"seeds": [format!("{base}/")], "table": "polite", "scope": "any", "per_host": 2, "delay_ms": 0, "sitemaps": false, "batch": 5}))).unwrap();
    let t0 = Instant::now();
    while job.active() && t0.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let s = job.status();
    local.stop().await;
    assert_eq!(s["phase"], "done", "{s:#}");
    let c = &s["counts"];
    assert_eq!(c["written"], 1 + 1 + 2 + 4, "{c:#}");
    assert_eq!((site.hits("/flaky"), c["asked_again"].as_u64(), c["misses"]["busy"].as_u64()), (3, Some(2), Some(2)), "{c:#}");
    let events = s["events"].as_array().unwrap();
    assert!(events.iter().any(|e| e["text"] == "127.0.0.1: HTTP 429: the site is busy and asks to wait 1 s; waits 1 s before its next request, then asks for the page again"), "{events:#?}");
    assert!(events.iter().any(|e| e["kind"] == "robots" && e["text"] == "localhost asks for 400 ms between requests"), "{events:#?}");
    let t = times.lock().unwrap().clone();
    assert_eq!(t.len(), 4);
    for w in t.windows(2) {
        assert!(w[1] - w[0] >= Duration::from_millis(380), "the other site's pages {:?} apart", w[1] - w[0]);
    }
}
