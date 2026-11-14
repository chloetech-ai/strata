//! How deep a crawl goes, served here (no Strata, so only what is fetched
//! and counted matters):
//!
//! - a list's next pages (`rel="next"`, or a link named "Next") are as deep
//!   as the list, so a long list is read to its end under the default
//!   depth (DAP-10: the default crawl of a shop stopped at list page 4);
//! - the links a crawl leaves out because of its depth are counted, so it
//!   can say that the depth stopped it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use chloedb_crawl::api::Service;
use chloedb_crawl::grow::{Grower, How, Policy};
use chloedb_crawl::job::Spec;
use chloedb_crawl::strata::Strata;
use serde_json::json;

const LISTS: u32 = 12;

#[derive(Clone, Default)]
struct Site {
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl Site {
    fn hit(&self, path: String) {
        *self.hits.lock().unwrap().entry(path).or_default() += 1;
    }
    fn hits(&self, prefix: &str) -> usize {
        self.hits.lock().unwrap().keys().filter(|p| p.starts_with(prefix)).count()
    }
}

fn html(title: &str, body: String) -> axum::response::Response {
    ([(header::CONTENT_TYPE, "text/html")], format!("<html><head><title>{title}</title></head><body>{body}</body></html>")).into_response()
}

/// `/list/1` .. `/list/12`, each with one item and the next list page
/// (by `rel="next"` on even pages, by its name on odd ones); an item links
/// to its details, which link one step further.
fn site(s: Site) -> Router {
    Router::new()
        .route("/robots.txt", get(|| async { "User-agent: *\nAllow: /\n" }))
        .route(
            "/list/:i",
            get(|State(s): State<Site>, Path(i): Path<u32>| async move {
                s.hit(format!("/list/{i}"));
                let next = match (i < LISTS, i % 2 == 0) {
                    (false, _) => String::new(),
                    (true, true) => format!(r#"<a rel="next" href="/list/{}">2</a>"#, i + 1),
                    (true, false) => format!(r#"<a href="/list/{}">Next &rsaquo;</a>"#, i + 1),
                };
                html(&format!("List {i}"), format!(r#"<a href="/item/{i}">Item {i}</a> {next}"#))
            }),
        )
        .route(
            "/item/:i",
            get(|State(s): State<Site>, Path(i): Path<u32>| async move {
                s.hit(format!("/item/{i}"));
                html(&format!("Item {i}"), format!(r#"<p>Item {i}, a thing.</p><a href="/details/{i}">details</a>"#))
            }),
        )
        .route(
            "/details/:i",
            get(|State(s): State<Site>, Path(i): Path<u32>| async move {
                s.hit(format!("/details/{i}"));
                html(&format!("Details {i}"), format!(r#"<p>Details of item {i}.</p><a href="/more/{i}">more</a>"#))
            }),
        )
        .with_state(s)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lists_next_pages_do_not_count_toward_the_depth_and_what_it_cut_is_counted() {
    chloedb_crawl::reach::set(chloedb_crawl::reach::Reach::any());
    let s = Site::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let app = site(s.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    // No Strata: the table cannot be made, but every page is fetched first
    // (the sample is larger than the site).
    let strata = Strata::new("http://127.0.0.1:9", None);
    let svc = Service::new(strata.clone(), Grower::new(Policy { how: How::Off, split_rows: 0, max_shards: 1 }, strata, None), None, false);
    let spec: Spec = serde_json::from_value(json!({"seeds": [format!("{base}/list/1")], "max_depth": 2, "delay_ms": 0, "per_host": 4, "sitemaps": false, "sample": 1000})).unwrap();
    let job = svc.start(spec).unwrap();
    let t0 = Instant::now();
    while job.active() {
        assert!(t0.elapsed() < Duration::from_secs(30), "{:#}", job.status());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let st = job.status();
    // Every list page (depth 0), item (1) and details page (2) ...
    assert_eq!((s.hits("/list/"), s.hits("/item/"), s.hits("/details/")), (LISTS as usize, LISTS as usize, LISTS as usize), "{st:#}");
    // ... and none of the pages past the depth, which are counted.
    assert_eq!(s.hits("/more/"), 0);
    assert_eq!(st["beyond_depth"], LISTS, "{st:#}");
}
