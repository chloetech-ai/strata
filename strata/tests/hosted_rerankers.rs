//! Cross-encoders served elsewhere (`strata::hosted::RerankEndpoint`),
//! against a mock server on loopback that scores as `builtin:overlap` does
//! and answers in each API's shape: a rerank phase sends its rows' text in
//! batches and ranks by the scores, whatever order they come back in; rate
//! limits and server errors are tried again; a refused key, a server that
//! keeps failing or a wrong answer fails the query; and a plan names the
//! endpoint it was made with.

#![cfg(feature = "hosted")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as J};
use strata::data::{self, col};
use strata::db::{Db, SqlResult};
use strata::query::exec::ResultSet;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

/// A request the mock took.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: J,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

type Answer = (u16, Vec<(String, String)>, String);
type Handler = dyn Fn(&Seen, usize) -> Answer + Send + Sync;

/// An HTTP server on loopback answering each request with `handler` (given
/// the request and how many came before it).
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    fn start(handler: impl Fn(&Seen, usize) -> Answer + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let s = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let (s, h) = (s.clone(), handler.clone());
                std::thread::spawn(move || serve(conn, &s, &*h));
            }
        });
        Mock { url, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn serve(conn: TcpStream, seen: &Mutex<Vec<Seen>>, handler: &Handler) {
    let mut r = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).unwrap();
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':').unwrap();
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = vec![0; len];
    r.read_exact(&mut body).unwrap();
    let req = Seen { path, headers, body: serde_json::from_slice(&body).unwrap_or(J::Null) };
    let n = {
        let mut s = seen.lock().unwrap();
        s.push(req.clone());
        s.len() - 1
    };
    let (status, extra, text) = handler(&req, n);
    let mut out = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n", text.len());
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&text);
    let mut w = conn;
    let _ = w.write_all(out.as_bytes());
}

/// What `builtin:overlap` scores: the share of the query's distinct words
/// in the text.
fn overlap(query: &str, text: &str) -> f64 {
    let words = |s: &str| s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect::<std::collections::HashSet<_>>();
    let (q, t) = (words(query), words(text));
    if q.is_empty() {
        return 0.0;
    }
    q.iter().filter(|w| t.contains(*w)).count() as f64 / q.len() as f64
}

/// The texts a request of `api` carries.
fn texts_of(api: &str, body: &J) -> Vec<String> {
    let key = if api == "tei" { "texts" } else { "documents" };
    body[key].as_array().unwrap_or_else(|| panic!("no {key} in {body}")).iter().map(|t| t.as_str().unwrap().to_string()).collect()
}

/// The mock's answer to a request of `api`: each text's overlap with the
/// query, best first, as the APIs order them.
fn answer(api: &str, body: &J) -> Answer {
    let q = body["query"].as_str().unwrap();
    let mut scored: Vec<(usize, f64)> = texts_of(api, body).iter().enumerate().map(|(i, t)| (i, overlap(q, t))).collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let list = |key: &str| scored.iter().map(|(i, x)| json!({"index": i, key: x})).collect::<Vec<_>>();
    let j = match api {
        "tei" => J::Array(list("score")),
        "voyage" => json!({"object": "list", "data": list("relevance_score"), "usage": {"total_tokens": 10}}),
        _ => json!({"id": "x", "results": list("relevance_score"), "meta": {"billed_units": {"search_units": 1}}}),
    };
    (200, Vec::new(), j.to_string())
}

fn docs() -> Vec<Vec<Value>> {
    data::synthetic(600, 16, 5)
}

/// A models directory holding `endpoint.json` = `config` for model `name`.
fn models_with(dir: &Path, name: &str, config: J) -> PathBuf {
    let models = dir.join("models");
    std::fs::create_dir_all(models.join(name)).unwrap();
    std::fs::write(models.join(name).join("endpoint.json"), config.to_string()).unwrap();
    models
}

/// A database of synthetic documents whose models directory is `models`.
fn create(path: &Path, models: &Path, docs: &[Vec<Value>]) -> Db {
    let db = Db::create(path, data::beir_schema(16, VectorIndexKind::Flat), DbOptions { segment_max_docs: 150, memtable_max_docs: 1_000_000, ..DbOptions::default() }).unwrap();
    db.open_embedders(models);
    db.load(docs.iter().cloned()).unwrap();
    db
}

fn query(db: &Db, text: &str, params: &Params) -> ResultSet {
    db.query(text, params).unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// A query of a few words of document `i`'s title, and of another's.
fn words(docs: &[Vec<Value>], i: usize) -> String {
    let title = |i: usize| docs[i % docs.len()][col::TITLE].as_str().unwrap().split(' ').take(2).collect::<Vec<_>>().join(" ");
    format!("{} {}", title(i), title(i * 7 + 3))
}

fn rerank(model: &str) -> String {
    format!("WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 30) SELECT id, title, score() FROM top ORDER BY cross_encoder('{model}', title, text, :t) DESC LIMIT 10")
}

#[test]
fn each_api_reranks_in_its_own_shape() {
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    std::env::set_var("STRATA_TEST_RERANK_KEY", "rk-1");
    let cases = [
        // (name, api as the mock answers, endpoint.json's api, path, batch)
        ("ours-cohere", "cohere", "cohere", "/v2/rerank", None),
        ("ours-jina", "jina", "jina", "/rerank", None),
        ("ours-voyage", "voyage", "voyage", "/rerank", None),
        ("ours-tei", "tei", "tei", "/rerank", Some(8)),
        ("ours-vllm", "cohere", "vllm", "/rerank", None),
    ];
    for (name, shape, api, path, batch) in cases {
        let mock = Mock::start(move |r, _| answer(shape, &r.body));
        let mut config = json!({"api": api, "base_url": mock.url, "model": "m-1", "key_env": "STRATA_TEST_RERANK_KEY", "headers": {"x-team": "search"}});
        if let Some(b) = batch {
            config["batch"] = json!(b);
        }
        let models = models_with(dir.path(), name, config);
        let db = create(&dir.path().join(format!("{name}.strata")), &models, &docs);
        for i in 0..5 {
            let params = Params::new().set("t", Value::Str(words(&docs, i * 13)));
            let want = query(&db, &rerank("builtin:overlap"), &params);
            let got = query(&db, &rerank(name), &params);
            assert_eq!((&got.keys, &got.scores), (&want.keys, &want.scores), "{name}");
            assert!(got.stats.plan.join("\n").contains(&format!("rerank 30 rows by cross_encoder('{name}', title, text)")), "{:?}", got.stats.plan);
        }
        let seen = mock.seen();
        // 30 rows a query, in batches of 8 for TEI (4 requests sent at once).
        assert_eq!(seen.len(), if batch.is_some() { 20 } else { 5 }, "{name}");
        for s in &seen {
            assert_eq!(s.path, path, "{name}");
            assert_eq!((s.header("authorization"), s.header("x-team")), (Some("Bearer rk-1"), Some("search")), "{name}");
            let b = &s.body;
            match api {
                "tei" => assert!(b["raw_scores"] == true && b["truncate"] == true && b.get("model").is_none(), "{b}"),
                "jina" => assert!(b["model"] == "m-1" && b["return_documents"] == false, "{b}"),
                "voyage" => assert!(b["model"] == "m-1" && b["truncation"] == true, "{b}"),
                _ => assert!(b["model"] == "m-1" && b.as_object().unwrap().len() == 3, "{b}"),
            }
        }
        let mut sent: Vec<usize> = seen.iter().map(|s| texts_of(shape, &s.body).len()).collect();
        sent.sort();
        assert_eq!(sent.iter().sum::<usize>(), 150, "{name}");
        assert!(sent.iter().all(|&n| n <= batch.unwrap_or(1000)), "{name}: {sent:?}");
    }
}

#[test]
fn a_provider_by_its_name() {
    let mock = Mock::start(|r, _| answer("cohere", &r.body));
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    // A base URL variable sends a provider's requests to a server of its
    // API, here the mock, which takes no key.
    std::env::set_var("COHERE_BASE_URL", &mock.url);
    let db = create(&dir.path().join("db.strata"), &dir.path().join("models"), &docs);
    let params = Params::new().set("t", Value::Str(words(&docs, 7)));
    let got = query(&db, &rerank("cohere/rerank-v3.5"), &params);
    assert_eq!(got.keys, query(&db, &rerank("builtin:overlap"), &params).keys);
    let seen = mock.seen();
    assert_eq!((seen.len(), seen[0].path.as_str(), seen[0].body["model"].as_str()), (1, "/v2/rerank", Some("rerank-v3.5")));
    // Without its key, a provider's own service is not called.
    if std::env::var_os("VOYAGE_API_KEY").is_none() && std::env::var_os("VOYAGE_BASE_URL").is_none() {
        let e = db.query(&rerank("voyage/rerank-2.5"), &params).unwrap_err().to_string();
        assert!(e.contains("cross-encoder voyage/rerank-2.5") && e.contains("$VOYAGE_API_KEY"), "{e}");
    }
    // A name of more than two parts, or one leaving the models directory,
    // is refused.
    for bad in ["a/b/c", "../m", "cohere/.."] {
        let e = db.query(&rerank(bad), &params).unwrap_err().to_string();
        assert!(e.contains("a ranking model's name is letters"), "{bad}: {e}");
    }
}

#[test]
fn rate_limits_are_tried_again_and_failures_fail_the_query() {
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    let params = Params::new().set("t", Value::Str(words(&docs, 3)));
    // Two rate limits, then answers.
    let mock = Mock::start(|r, n| match n {
        0 | 1 => (429, vec![("retry-after".into(), "0".into())], json!({"message": "too many requests"}).to_string()),
        _ => answer("tei", &r.body),
    });
    let models = models_with(dir.path(), "busy", json!({"api": "tei", "url": format!("{}/rerank", mock.url)}));
    let db = create(&dir.path().join("a.strata"), &models, &docs);
    assert_eq!(query(&db, &rerank("busy"), &params).keys, query(&db, &rerank("builtin:overlap"), &params).keys);
    assert_eq!(mock.seen().len(), 3);

    // A server that keeps failing fails the query, after two more tries.
    let mock = Mock::start(|_, _| (503, Vec::new(), "upstream down".into()));
    let models = models_with(dir.path(), "down", json!({"api": "tei", "url": format!("{}/rerank", mock.url)}));
    let db = create(&dir.path().join("b.strata"), &models, &docs);
    let e = db.query(&rerank("down"), &params).unwrap_err();
    assert!(matches!(e, strata::Error::Unavailable(_)), "{e}");
    assert!(e.to_string().contains("answered 503: upstream down (after 3 tries)"), "{e}");
    assert_eq!(mock.seen().len(), 3);

    // A refused key fails at once, and the key is not shown.
    std::env::set_var("STRATA_TEST_RERANK_SECRET", "rk-very-secret");
    let mock = Mock::start(|_, _| (401, Vec::new(), json!({"message": "invalid api token"}).to_string()));
    let models = models_with(dir.path(), "refused", json!({"api": "cohere", "url": format!("{}/v2/rerank", mock.url), "key_env": "STRATA_TEST_RERANK_SECRET"}));
    let db = create(&dir.path().join("c.strata"), &models, &docs);
    let e = db.query(&rerank("refused"), &params).unwrap_err().to_string();
    assert!(e.contains("401") && e.contains("invalid api token") && e.contains("$STRATA_TEST_RERANK_SECRET"), "{e}");
    assert!(!e.contains("rk-very-secret"), "{e}");
    assert_eq!(mock.seen().len(), 1);
}

#[test]
fn wrong_answers_fail_the_query() {
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    let params = Params::new().set("t", Value::Str(words(&docs, 3)));
    let wrong: [(&str, fn(&J) -> J, &str); 4] = [
        ("short", |b| json!({"results": [{"index": 0, "relevance_score": 0.5}], "n": texts_of("cohere", b).len()}), "answered 1 scores for 30 texts"),
        ("twice", |_| json!({"results": [{"index": 0, "relevance_score": 0.5}, {"index": 0, "relevance_score": 0.4}]}), "or twice"),
        ("beyond", |_| json!({"results": [{"index": 99, "relevance_score": 0.5}]}), "text 99 of 30"),
        ("shapeless", |_| json!({"data": []}), "without a list of scores"),
    ];
    for (name, make, says) in wrong {
        let mock = Mock::start(move |r, _| (200, Vec::new(), make(&r.body).to_string()));
        let models = models_with(dir.path(), name, json!({"api": "cohere", "url": format!("{}/v2/rerank", mock.url)}));
        let db = create(&dir.path().join(format!("{name}.strata")), &models, &docs);
        let e = db.query(&rerank(name), &params).unwrap_err().to_string();
        assert!(e.contains(says), "{name}: {e}");
    }
}

#[test]
fn a_plan_names_the_endpoint_it_was_made_with() {
    let mock = Mock::start(|r, _| answer("tei", &r.body));
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    let models = models_with(dir.path(), "gpu", json!({"api": "tei", "url": format!("{}/rerank", mock.url)}));
    let db = create(&dir.path().join("db.strata"), &models, &docs);
    let params = Params::new().set("t", Value::Str(words(&docs, 11)));
    let plan = db.explain(&rerank("gpu"), &params).unwrap();
    let text = plan.explain(&[&db.schema()]).join("\n");
    let hash = text.split("ranking model: gpu (hash ").nth(1).and_then(|t| t.split(')').next()).unwrap_or_else(|| panic!("{text}")).to_string();
    assert_eq!(hash.len(), 16, "{text}");
    let SqlResult::Rows(a) = db.run(&plan).unwrap() else { panic!("rows") };
    assert_eq!(a.keys, query(&db, &rerank("builtin:overlap"), &params).keys);
    // Another server is another model: the plan made with the first is
    // refused, and a new one runs.
    std::fs::write(models.join("gpu/endpoint.json"), json!({"api": "tei", "url": format!("{}/v2/rerank", mock.url)}).to_string()).unwrap();
    let e = db.run(&plan).err().expect("refused").to_string();
    assert!(e.contains("ranking model gpu here is not the one the plan was made with"), "{e}");
    let again = db.explain(&rerank("gpu"), &params).unwrap();
    assert!(!again.explain(&[&db.schema()]).join("\n").contains(&hash));
}
