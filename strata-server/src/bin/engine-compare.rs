//! Strata, Elasticsearch and Solr on the same BEIR collection, with the same
//! shards and the same CPU and memory (see `deploy/compare/README.md`):
//! how long each takes to load it, how large its index is, and how fast and
//! how well it answers the query kinds of `strata-server-bench`.
//!
//! ```text
//! engine-compare build --shard 0 --of 2 --out /local/quora.shard0of2.strata   # Strata: one shard, offline
//! engine-compare load --engine es --url http://es-0.es:9200,http://es-1.es:9200 --out /data/compare
//! engine-compare run --engine es --url http://es-0.es:9200,http://es-1.es:9200 --out /data/compare
//! ```
//!
//! `build` and `load` read the collection prepared by
//! `strata/scripts/prepare_beir.py` as a stream (`$STRATA_DATA`), so they
//! fit in a pod's memory. `load` creates the index (Elasticsearch) or the
//! collection and its schema (Solr) first, untimed, then times the bulk
//! requests and the merge of every shard into one segment, as Strata's
//! databases are built. `run` sends each kind's requests once (recording
//! the answers, for `deploy/compare/analyze.py` to score), then times them
//! for `--seconds` at each `--concurrency`, sending requests round-robin to
//! the `--url`s.
//!
//! Each kind is the closest native request of each engine; where an engine
//! has none, the kind is left out of its report with the reason
//! (`unavailable`), and where its closest request means something else, the
//! report says so (`differs`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::{json, Value as Json};
use strata::data::{self, col};
use strata::db::Db;
use strata::metrics::{mean, percentile};
use strata::segment::build::BuildOptions;
use strata::storage::DbOptions;
use strata::{Document, Value, VectorCodes, VectorIndexKind};
use strata_server::server;
use strata_server::service::shard_of;

#[derive(Parser)]
#[command(about = "Strata, Elasticsearch and Solr on the same BEIR collection")]
struct Cli {
    /// A collection under $STRATA_DATA.
    #[arg(long, global = true, default_value = "quora")]
    collection: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Builds shard I of N into one Strata database (one segment, HNSW, 8-bit codes), timed.
    Build {
        #[arg(long)]
        shard: usize,
        #[arg(long)]
        of: usize,
        #[arg(long)]
        out: PathBuf,
        /// Threads for building the vector index.
        #[arg(long, default_value_t = 2)]
        threads: usize,
    },
    /// Loads the collection into Elasticsearch or Solr over HTTP and merges each shard to one segment, timed.
    Load {
        #[arg(long, value_enum)]
        engine: Engine,
        /// The engine's nodes, comma-separated; bulk requests go round-robin.
        #[arg(long, value_delimiter = ',', required = true)]
        url: Vec<String>,
        #[arg(long, default_value_t = 2)]
        shards: usize,
        /// Documents per bulk request.
        #[arg(long, default_value_t = 500)]
        batch: usize,
        /// Bulk requests in flight.
        #[arg(long, default_value_t = 4)]
        clients: usize,
        /// Load only the first N documents (for a quick check).
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Times every query kind against an engine and records its answers.
    Run {
        #[arg(long, value_enum)]
        engine: Engine,
        /// The engine's HTTP endpoints, comma-separated (Strata: its coordinator); requests go round-robin.
        #[arg(long, value_delimiter = ',', required = true)]
        url: Vec<String>,
        #[arg(long, default_value_t = 300)]
        queries: usize,
        #[arg(long, value_delimiter = ',', default_value = "1,4,16")]
        concurrency: Vec<usize>,
        #[arg(long, default_value_t = 6.0)]
        seconds: f64,
        /// Names the run in the report (`--out/run-<label>.json`).
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Engine {
    Strata,
    Es,
    Solr,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Engine::Strata => "strata",
            Engine::Es => "es",
            Engine::Solr => "solr",
        }
    }
}

fn s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Deserialize)]
struct Meta {
    docs: usize,
    dim: usize,
}

fn meta(collection: &str) -> Result<Meta, String> {
    let path = data::data_dir().join(collection).join("meta.json");
    serde_json::from_reader(File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?).map_err(s)
}

#[derive(Deserialize)]
struct RawDoc {
    id: String,
    title: String,
    text: String,
    year: i64,
    category: String,
    rating: f64,
    topic: String,
}

impl RawDoc {
    /// The searched text, as `data::load` makes it.
    fn body(&self) -> String {
        if self.title.is_empty() {
            self.text.clone()
        } else {
            format!("{}\n{}", self.title, self.text)
        }
    }
}

/// Calls `f` with every document of the collection (or the first `limit`)
/// and its vector, reading both files as a stream.
fn each_doc(collection: &str, limit: Option<usize>, mut f: impl FnMut(RawDoc, Vec<f32>) -> Result<(), String>) -> Result<usize, String> {
    let dir = data::data_dir().join(collection);
    let m = meta(collection)?;
    let mut vecs = BufReader::with_capacity(1 << 20, File::open(dir.join("doc_vecs.f32")).map_err(s)?);
    let docs = BufReader::with_capacity(1 << 20, File::open(dir.join("docs.jsonl")).map_err(s)?);
    let mut row = vec![0u8; m.dim * 4];
    let mut n = 0;
    for line in docs.lines().take(limit.unwrap_or(usize::MAX)) {
        let r: RawDoc = serde_json::from_str(&line.map_err(s)?).map_err(s)?;
        vecs.read_exact(&mut row).map_err(s)?;
        f(r, row.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())?;
        n += 1;
    }
    if n != m.docs.min(limit.unwrap_or(usize::MAX)) {
        return Err(format!("read {n} documents, meta.json says {}", m.docs));
    }
    Ok(n)
}

fn build(collection: &str, shard: usize, of: usize, out: &Path, threads: usize) -> Result<(), String> {
    let t = Instant::now();
    let dim = meta(collection)?.dim;
    let mut docs: Vec<Document> = Vec::new();
    each_doc(collection, None, |r, v| {
        if shard_of(&Value::Str(r.id.clone()), of) == shard {
            let body = r.body();
            docs.push(vec![
                Value::Str(r.id),
                Value::Str(r.title),
                Value::Str(r.text),
                Value::Str(body),
                Value::Int(r.year),
                Value::Str(r.category),
                Value::Float(r.rating),
                Value::Str(r.topic),
                Value::Vector(v.into()),
            ]);
        }
        Ok(())
    })?;
    let read_s = t.elapsed().as_secs_f64();
    let n = docs.len();
    for p in [out.to_path_buf(), PathBuf::from(format!("{}-wal", out.display()))] {
        if p.exists() {
            std::fs::remove_file(&p).map_err(s)?;
        }
    }
    // As strata-server-bench builds its databases.
    let mut schema = data::beir_schema(dim, VectorIndexKind::Hnsw { m: 16, ef_construction: 200 });
    schema.fields[col::EMB].codes = VectorCodes::Sq8;
    let opts = DbOptions { build: BuildOptions::default().set("vector.threads", threads), segment_max_docs: n.max(1), ..server::db_options(false) };
    let t1 = Instant::now();
    let db = Db::create(out, schema, opts).map_err(s)?;
    db.load(docs).map_err(s)?;
    let build_s = t1.elapsed().as_secs_f64();
    let file_bytes = db.storage().file_stats().file_bytes;
    drop(db);
    let report = json!({
        "engine": "strata", "shard": shard, "of": of, "docs": n, "threads": threads,
        "read_s": read_s, "build_s": build_s, "total_s": t.elapsed().as_secs_f64(), "file_bytes": file_bytes,
    });
    std::fs::write(format!("{}.build.json", out.display()), report.to_string()).map_err(s)?;
    println!("{report}");
    Ok(())
}

// --- Loading Elasticsearch and Solr --------------------------------------

/// Lucene's classic English stopwords, which Strata drops from documents
/// (`strata::text::analyzer::STOPWORDS`).
const CLASSIC_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it", "no", "not", "of", "on",
    "or", "such", "that", "the", "their", "then", "there", "these", "they", "this", "to", "was", "will", "with",
];

/// The analysis of Strata's `body` field: words are runs of letters and
/// digits, lowercased and folded to ASCII, minus the classic stopwords,
/// stemmed with Snowball English (Porter 2); ranked queries also drop
/// `QUERY_STOPWORDS`.
fn es_index() -> Json {
    let query_stop: Vec<&str> = strata::text::analyzer::QUERY_STOPWORDS.to_vec();
    json!({
        "settings": {
            "number_of_shards": 2, "number_of_replicas": 0, "refresh_interval": "-1",
            "analysis": {
                "tokenizer": {"words": {"type": "pattern", "pattern": "[^\\p{L}\\p{N}\\p{M}]+"}},
                "filter": {
                    "classic_stop": {"type": "stop", "stopwords": CLASSIC_STOPWORDS},
                    "query_stop": {"type": "stop", "stopwords": query_stop},
                    "porter2": {"type": "snowball", "language": "English"},
                },
                "analyzer": {
                    "strata": {"type": "custom", "tokenizer": "words", "filter": ["lowercase", "asciifolding", "classic_stop", "porter2"]},
                    "strata_query": {"type": "custom", "tokenizer": "words", "filter": ["lowercase", "asciifolding", "query_stop", "classic_stop", "porter2"]},
                },
            },
        },
        "mappings": {
            "_source": {"excludes": ["body", "emb"]},
            "properties": {
                "id": {"type": "keyword"},
                "title": {"type": "text", "index": false},
                "text": {"type": "text", "index": false},
                "body": {"type": "text", "analyzer": "strata", "search_analyzer": "strata_query"},
                "year": {"type": "integer"},
                "category": {"type": "keyword"},
                "rating": {"type": "float"},
                "topic": {"type": "keyword"},
                "emb": {
                    "type": "dense_vector", "dims": 1024, "index": true, "similarity": "dot_product",
                    "index_options": {"type": "int8_hnsw", "m": 16, "ef_construction": 200},
                },
            },
        },
    })
}

fn solr_text_type() -> Json {
    let tokenizer = json!({"class": "solr.PatternTokenizerFactory", "pattern": "[^\\p{L}\\p{N}\\p{M}]+"});
    let filters = |query: bool| {
        let mut f = vec![json!({"class": "solr.LowerCaseFilterFactory"}), json!({"class": "solr.ASCIIFoldingFilterFactory"})];
        if query {
            f.push(json!({"class": "solr.ManagedStopFilterFactory", "managed": "query_en"}));
        }
        f.push(json!({"class": "solr.ManagedStopFilterFactory", "managed": "classic_en"}));
        f.push(json!({"class": "solr.SnowballPorterFilterFactory", "language": "English"}));
        f
    };
    json!({
        "name": "text_strata", "class": "solr.TextField", "positionIncrementGap": "100",
        "indexAnalyzer": {"tokenizer": tokenizer, "filters": filters(false)},
        "queryAnalyzer": {"tokenizer": tokenizer, "filters": filters(true)},
    })
}

async fn check(r: reqwest::Result<reqwest::Response>, what: &str) -> Result<Json, String> {
    let r = r.map_err(|e| format!("{what}: {e}"))?;
    let status = r.status();
    let text = r.text().await.map_err(s)?;
    if !status.is_success() {
        return Err(format!("{what}: {status}: {}", text.chars().take(2000).collect::<String>()));
    }
    Ok(serde_json::from_str(&text).unwrap_or(Json::Null))
}

/// Creates the empty index (Elasticsearch) or collection (Solr), dropping
/// one a previous run left.
async fn setup(engine: Engine, client: &reqwest::Client, url: &str, shards: usize) -> Result<(), String> {
    match engine {
        Engine::Es => {
            let _ = client.delete(format!("{url}/quora")).send().await;
            let mut body = es_index();
            body["settings"]["number_of_shards"] = json!(shards);
            check(client.put(format!("{url}/quora")).json(&body).send().await, "create index").await?;
            check(client.get(format!("{url}/_cluster/health/quora?wait_for_status=green&timeout=120s")).send().await, "index health").await?;
        }
        Engine::Solr => {
            let admin = format!("{url}/solr/admin/collections");
            let _ = client.get(format!("{admin}?action=DELETE&name=quora")).send().await;
            // Without collection.configName, Solr copies _default into its own configset.
            check(client.get(format!("{admin}?action=CREATE&name=quora&numShards={shards}&replicationFactor=1")).send().await, "create collection").await?;
            let c = format!("{url}/solr/quora");
            check(
                client
                    .post(format!("{c}/config"))
                    .json(&json!({
                        // No guessed fields; no searchers opened while loading (as Elasticsearch's refresh_interval -1);
                        // no cached result lists, since the timed runs repeat the same queries.
                        "set-user-property": {"update.autoCreateFields": "false"},
                        "set-property": {"updateHandler.autoSoftCommit.maxTime": -1, "query.queryResultCache.size": 0},
                        "add-searchcomponent": {"name": "combined_query", "class": "solr.CombinedQueryComponent"},
                        "add-requesthandler": {"name": "/hybrid", "class": "solr.CombinedQuerySearchHandler"},
                    }))
                    .send()
                    .await,
                "config",
            )
            .await?;
            // Types first, in a request of their own (serde_json orders an object's keys).
            check(
                client
                    .post(format!("{c}/schema"))
                    .json(&json!({"add-field-type": [
                        solr_text_type(),
                        {"name": "knn_1024", "class": "solr.DenseVectorField", "vectorDimension": 1024, "similarityFunction": "dot_product",
                         "knnAlgorithm": "hnsw", "hnswM": 16, "hnswEfConstruction": 200},
                    ]}))
                    .send()
                    .await,
                "schema types",
            )
            .await?;
            check(
                client
                    .post(format!("{c}/schema"))
                    .json(&json!({
                        "add-field": [
                            {"name": "title", "type": "string", "indexed": false, "docValues": false, "stored": true},
                            {"name": "text", "type": "string", "indexed": false, "docValues": false, "stored": true},
                            {"name": "body", "type": "text_strata", "stored": false},
                            {"name": "year", "type": "pint", "stored": true},
                            {"name": "category", "type": "string", "stored": true},
                            {"name": "rating", "type": "pfloat", "stored": true},
                            {"name": "topic", "type": "string", "stored": true},
                            {"name": "emb", "type": "knn_1024", "stored": false},
                        ],
                    }))
                    .send()
                    .await,
                "schema",
            )
            .await?;
            let query_stop: Vec<&str> = strata::text::analyzer::QUERY_STOPWORDS.to_vec();
            for (name, words) in [("classic_en", CLASSIC_STOPWORDS.to_vec()), ("query_en", query_stop)] {
                check(client.put(format!("{c}/schema/analysis/stopwords/{name}")).json(&words).send().await, "stopwords").await?;
            }
            check(client.get(format!("{admin}?action=RELOAD&name=quora")).send().await, "reload").await?;
        }
        Engine::Strata => return Err("Strata is built with `build`".into()),
    }
    Ok(())
}

/// One document as JSON, its vector written with the shortest decimal that
/// reads back as the same f32.
fn doc_json(r: &RawDoc, v: &[f32]) -> String {
    let mut out = serde_json::to_string(&json!({
        "id": r.id, "title": r.title, "text": r.text, "body": r.body(),
        "year": r.year, "category": r.category, "rating": r.rating, "topic": r.topic,
    }))
    .unwrap();
    out.pop();
    out.push_str(",\"emb\":[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write!(out, "{x}").unwrap();
    }
    out.push_str("]}");
    out
}

/// Posts one bulk request, again after a pause while the engine pushes back.
async fn post_batch(engine: Engine, client: &reqwest::Client, url: &str, body: Arc<Vec<u8>>) -> Result<(), String> {
    let (path, ctype) = match engine {
        Engine::Es => ("/quora/_bulk", "application/x-ndjson"),
        _ => ("/solr/quora/update", "application/json"),
    };
    let mut tries = 0;
    loop {
        tries += 1;
        let r = client.post(format!("{url}{path}")).header("content-type", ctype).body(body.as_ref().clone()).send().await;
        let retry = match &r {
            Ok(r) => r.status().as_u16() == 429 || r.status().as_u16() == 503,
            Err(e) => e.is_connect() || e.is_timeout(),
        };
        if retry && tries < 20 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let j = check(r, "bulk").await?;
        if engine == Engine::Es && j["errors"].as_bool() == Some(true) {
            let first = j["items"].as_array().and_then(|items| items.iter().find(|i| i["index"].get("error").is_some()).cloned());
            return Err(format!("bulk item failed: {}", first.unwrap_or(Json::Null)));
        }
        return Ok(());
    }
}

#[allow(clippy::too_many_arguments)]
async fn load(collection: &str, engine: Engine, urls: &[String], shards: usize, batch: usize, clients: usize, limit: Option<usize>, out: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(6 * 3600)).build().map_err(s)?;
    let t_setup = Instant::now();
    setup(engine, &client, &urls[0], shards).await?;
    let setup_s = t_setup.elapsed().as_secs_f64();
    eprintln!("{}: index created in {setup_s:.1} s", engine.name());

    let t = Instant::now();
    // Bodies are built on a thread, `clients` requests at a time in flight.
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(clients * 2);
    let coll = collection.to_string();
    let producer = std::thread::spawn(move || -> Result<(usize, u64), String> {
        let (mut body, mut in_batch, mut bytes) = (Vec::<u8>::new(), 0usize, 0u64);
        let finish = |body: &mut Vec<u8>| {
            if engine == Engine::Solr {
                body.push(b']');
            }
            std::mem::take(body)
        };
        let n = each_doc(&coll, limit, |r, v| {
            match engine {
                Engine::Es => {
                    body.extend_from_slice(serde_json::to_string(&json!({"index": {"_id": r.id}})).unwrap().as_bytes());
                    body.push(b'\n');
                    body.extend_from_slice(doc_json(&r, &v).as_bytes());
                    body.push(b'\n');
                }
                _ => {
                    body.push(if in_batch == 0 { b'[' } else { b',' });
                    body.extend_from_slice(doc_json(&r, &v).as_bytes());
                }
            }
            in_batch += 1;
            if in_batch == batch {
                let b = finish(&mut body);
                bytes += b.len() as u64;
                tx.blocking_send(b).map_err(s)?;
                in_batch = 0;
            }
            Ok(())
        })?;
        if in_batch > 0 {
            let b = finish(&mut body);
            bytes += b.len() as u64;
            tx.blocking_send(b).map_err(s)?;
        }
        Ok((n, bytes))
    });
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let sent = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for c in 0..clients {
        let (rx, client, sent, urls) = (rx.clone(), client.clone(), sent.clone(), urls.to_vec());
        tasks.push(tokio::spawn(async move {
            loop {
                let Some(body) = rx.lock().await.recv().await else { return Ok::<(), String>(()) };
                let i = sent.fetch_add(1, Ordering::Relaxed);
                post_batch(engine, &client, &urls[(i + c) % urls.len()], Arc::new(body)).await?;
                let docs = (i + 1) * batch;
                if docs % 25_000 < batch {
                    eprintln!("{}: {docs} documents sent, {:.0} s", engine.name(), t.elapsed().as_secs_f64());
                }
            }
        }));
    }
    for task in tasks {
        task.await.map_err(s)??;
    }
    let (docs, bytes) = producer.join().map_err(|_| "reader thread panicked".to_string())??;
    let ingest_s = t.elapsed().as_secs_f64();
    eprintln!("{}: {docs} documents ({:.1} GB of JSON) sent in {ingest_s:.1} s", engine.name(), bytes as f64 / 1e9);

    // Make it searchable, then merge every shard to one segment.
    let u = &urls[0];
    let t1 = Instant::now();
    match engine {
        Engine::Es => check(client.post(format!("{u}/quora/_refresh")).send().await, "refresh").await?,
        _ => check(client.get(format!("{u}/solr/quora/update?commit=true")).send().await, "commit").await?,
    };
    let refresh_s = t1.elapsed().as_secs_f64();
    let t2 = Instant::now();
    match engine {
        Engine::Es => check(client.post(format!("{u}/quora/_forcemerge?max_num_segments=1")).send().await, "force merge").await?,
        _ => check(client.get(format!("{u}/solr/quora/update?optimize=true&maxSegments=1&waitSearcher=true")).send().await, "optimize").await?,
    };
    if engine == Engine::Es {
        check(client.post(format!("{u}/quora/_refresh")).send().await, "refresh").await?;
    }
    let merge_s = t2.elapsed().as_secs_f64();
    let total_s = t.elapsed().as_secs_f64();
    eprintln!("{}: searchable after {refresh_s:.1} s more, merged in {merge_s:.1} s; {total_s:.1} s in all", engine.name());

    let mut report = json!({
        "engine": engine.name(), "collection": collection, "docs": docs, "json_bytes": bytes, "batch": batch, "clients": clients,
        "setup_s": setup_s, "ingest_s": ingest_s, "refresh_s": refresh_s, "merge_s": merge_s, "total_s": total_s,
    });
    // The index as stored: its size, documents and segments, per shard.
    match engine {
        Engine::Es => {
            let st = check(client.get(format!("{u}/quora/_stats/store,docs,segments?level=shards")).send().await, "stats").await?;
            report["index_bytes"] = st["_all"]["primaries"]["store"]["size_in_bytes"].clone();
            report["indexed_docs"] = st["_all"]["primaries"]["docs"]["count"].clone();
            report["segments"] = st["_all"]["primaries"]["segments"]["count"].clone();
            let c = check(client.get(format!("{u}/quora/_count")).send().await, "count").await?;
            report["searchable_docs"] = c["count"].clone();
            let du = check(client.post(format!("{u}/quora/_disk_usage?run_expensive_tasks=true")).send().await, "disk usage").await?;
            report["disk_usage"] = du["quora"].clone();
        }
        _ => {
            let (mut size, mut n, mut segs, mut cores) = (0u64, 0u64, 0u64, Vec::new());
            for node in urls {
                let st = check(client.get(format!("{node}/solr/admin/cores?action=STATUS&wt=json")).send().await, "core status").await?;
                for (name, core) in st["status"].as_object().cloned().unwrap_or_default() {
                    if name.starts_with("quora") {
                        size += core["index"]["sizeInBytes"].as_u64().unwrap_or(0);
                        n += core["index"]["numDocs"].as_u64().unwrap_or(0);
                        segs += core["index"]["segmentCount"].as_u64().unwrap_or(0);
                        cores.push(json!({"core": name, "index": core["index"]}));
                    }
                }
            }
            report["index_bytes"] = json!(size);
            report["indexed_docs"] = json!(n);
            report["segments"] = json!(segs);
            report["cores"] = json!(cores);
            let c = check(client.get(format!("{u}/solr/quora/select?q=*:*&rows=0")).send().await, "count").await?;
            report["searchable_docs"] = c["response"]["numFound"].clone();
        }
    }
    if report["searchable_docs"].as_u64() != Some(docs as u64) {
        eprintln!("{}: WARNING: {docs} documents sent, {} searchable", engine.name(), report["searchable_docs"]);
    }
    std::fs::create_dir_all(out).map_err(s)?;
    let path = out.join(format!("load-{}.json", engine.name()));
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).map_err(s)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

// --- Query kinds ---------------------------------------------------------

/// The kinds of `strata-server-bench`, as Strata SQL.
const KINDS: &[(&str, &str)] = &[
    ("bm25", "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10"),
    ("bm25 + filter", "SELECT id, score() FROM docs WHERE year >= 2010 AND category IN ('c00', 'c01', 'c02', 'c03') ORDER BY bm25(body, :t) DESC LIMIT 10"),
    ("vector", "SELECT id, score() FROM docs ORDER BY similarity(emb, :v) DESC LIMIT 10"),
    ("vector + filter", "SELECT id, score() FROM docs WHERE rating >= 4 ORDER BY similarity(emb, :v) DESC LIMIT 10"),
    ("hybrid (RRF)", "SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :v)) DESC LIMIT 10"),
    ("hybrid (weighted)", "SELECT id, score() FROM docs ORDER BY 0.2 * bm25(body, :t) + 0.8 * similarity(emb, :v) DESC LIMIT 10"),
    ("boosted (bm25 × decay)", "SELECT id, score() FROM docs ORDER BY bm25(body, :t) * decay(year, 2024, 5) DESC LIMIT 10"),
    ("facet", "SELECT category, COUNT(*) FROM docs WHERE MATCH(body, :w) GROUP BY category ORDER BY 2 DESC LIMIT 5"),
    ("key lookup", "SELECT id, title, year, rating FROM docs WHERE id = :k"),
];

/// HNSW candidates for the vector kinds: Strata searches with ef 64 and
/// re-scores the best 40 with the full vectors; Elasticsearch gets the same
/// (num_candidates 64, rescore_vector oversample 4); Solr's knn has no
/// re-scoring (its vectors are full precision) and searches topK 64.
const CANDIDATES: usize = 64;
const OVERSAMPLE: usize = 4;
/// Each input's depth for the hybrid kinds (Strata's default).
const DEPTH: usize = 100;
/// ln(0.5) / 5: `decay(year, 2024, 5)`, exponential, halving every 5 years.
const DECAY_RATE: f64 = -0.13862943611198906;

/// The longest ASCII word of `s` with at least 4 letters (as strata-server-bench picks it).
fn match_word(s: &str) -> Option<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 4).max_by_key(|w| w.len()).map(|w| w.to_lowercase())
}

/// The words of `s`, lowercased and space-separated: what every engine's
/// analysis splits it into anyway, without characters the Lucene query
/// parser would read as syntax (or upper-case AND, OR, NOT).
fn words(s: &str) -> String {
    let w: Vec<String> = s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect();
    if w.is_empty() {
        "_".into()
    } else {
        w.join(" ")
    }
}

fn vector_text(v: &[f32]) -> String {
    let mut out = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write!(out, "{x}").unwrap();
    }
    out.push(']');
    out
}

/// What a kind means on an engine, where that is not what it means on Strata.
fn differs(engine: Engine, kind: &str) -> Option<&'static str> {
    match (engine, kind) {
        (Engine::Es, "hybrid (weighted)") => Some(
            "no min-max normalised fusion on the free licence (the linear retriever needs a paid one): the sum of 0.2 x the raw BM25 score and 0.8 x the vector score ((1 + dot) / 2) over the lexical matches and the knn top 100",
        ),
        (Engine::Solr, "hybrid (weighted)") => Some(
            "no min-max normalised fusion: the sum of 0.2 x the raw BM25 score (scaled by k1 + 1 = 2.2 as Elasticsearch's) and 0.8 x the vector score ((1 + dot) / 2) over the lexical matches and the knn top 100",
        ),
        (Engine::Es, "hybrid (RRF)") => Some("the rrf retriever"),
        (Engine::Solr, "hybrid (RRF)") => Some("the combined query handler, rrf, k 60, knn topK 100; the lexical query's depth is the request's limit"),
        _ => None,
    }
}

/// The request for one kind on one engine: the path under the engine's URL
/// and the JSON body.
fn request(engine: Engine, kind: &str, sql: &str, q: &data::Query, key: &str) -> Option<(&'static str, Json)> {
    let t = &q.text;
    // Facets need a word of the query (as strata-server-bench, they skip queries without one).
    let w = if sql.contains(":w") { match_word(t)? } else { String::new() };
    // MATCH(body, :w) analyses the word as documents are (no query
    // stopwords): the others look up that term as it is.
    let term = strata::text::analyzer::analyze(&w).into_iter().next().unwrap_or_else(|| w.clone());
    let v = &q.vector;
    match engine {
        Engine::Strata => {
            let mut p = serde_json::Map::new();
            if sql.contains(":t") {
                p.insert("t".into(), json!(t));
            }
            if sql.contains(":v") {
                p.insert("v".into(), json!(v.to_vec()));
            }
            if sql.contains(":w") {
                p.insert("w".into(), json!(&w));
            }
            if sql.contains(":k") {
                p.insert("k".into(), json!(key));
            }
            Some(("/v1/query", json!({"sql": sql, "params": p})))
        }
        Engine::Es => {
            let m = json!({"match": {"body": t}});
            let knn = |k: usize, n: usize| json!({"field": "emb", "query_vector": v.to_vec(), "k": k, "num_candidates": n});
            let top = |extra: Json| {
                let mut b = json!({"size": 10, "_source": false, "track_total_hits": false});
                b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                b
            };
            let body = match kind {
                "bm25" => top(json!({"query": m})),
                "bm25 + filter" => top(json!({"query": {"bool": {"must": [m], "filter": [
                    {"range": {"year": {"gte": 2010}}}, {"terms": {"category": ["c00", "c01", "c02", "c03"]}}]}}})),
                "vector" | "vector + filter" => {
                    let mut k = knn(10, CANDIDATES);
                    k["rescore_vector"] = json!({"oversample": OVERSAMPLE});
                    if kind == "vector + filter" {
                        k["filter"] = json!({"range": {"rating": {"gte": 4}}});
                    }
                    top(json!({"knn": k}))
                }
                "hybrid (RRF)" => json!({"size": 10, "_source": false, "retriever": {"rrf": {
                    "retrievers": [{"standard": {"query": m}}, {"knn": knn(DEPTH, DEPTH)}],
                    "rank_window_size": DEPTH, "rank_constant": 60}}}),
                "hybrid (weighted)" => {
                    let mut k = knn(DEPTH, DEPTH);
                    k["boost"] = json!(0.8);
                    top(json!({"query": {"match": {"body": {"query": t, "boost": 0.2}}}, "knn": k}))
                }
                "boosted (bm25 × decay)" => top(json!({"query": {"function_score": {"query": m,
                    "functions": [{"exp": {"year": {"origin": 2024, "scale": 5, "decay": 0.5}}}], "boost_mode": "multiply"}}})),
                "facet" => json!({"size": 0, "track_total_hits": false, "query": {"term": {"body": term}},
                    "aggs": {"c": {"terms": {"field": "category", "size": 5}}}}),
                "key lookup" => json!({"size": 10, "track_total_hits": false, "_source": ["title", "year", "rating"], "query": {"term": {"id": key}}}),
                _ => return None,
            };
            // The shard request cache would answer repeated size-0 requests (facets) from memory.
            Some(("/quora/_search?request_cache=false", body))
        }
        Engine::Solr => {
            let lex = format!("{{!lucene df=body q.op=OR}}{}", words(t));
            let knn = |k: usize| format!("{{!knn f=emb topK={k}}}{}", vector_text(v));
            let ranked = |query: Json, params: Json| {
                let mut p = json!({"minExactCount": 10});
                p.as_object_mut().unwrap().extend(params.as_object().cloned().unwrap_or_default());
                json!({"query": query, "limit": 10, "fields": "id,score", "params": p})
            };
            let body = match kind {
                "bm25" => ranked(json!(lex), json!({})),
                "bm25 + filter" => {
                    let mut b = ranked(json!(lex), json!({}));
                    b["filter"] = json!(["year:[2010 TO *]", "{!terms f=category}c00,c01,c02,c03"]);
                    b
                }
                "vector" | "vector + filter" => {
                    let mut b = json!({"query": knn(CANDIDATES), "limit": 10, "fields": "id,score"});
                    if kind == "vector + filter" {
                        b["filter"] = json!(["rating:[4 TO *]"]);
                    }
                    b
                }
                "hybrid (RRF)" => {
                    return Some((
                        "/solr/quora/hybrid",
                        json!({
                            "queries": {"lex": {"lucene": {"query": words(t), "df": "body", "q.op": "OR"}},
                                        "vec": {"knn": {"f": "emb", "topK": DEPTH, "query": vector_text(v)}}},
                            "limit": 10, "fields": "id,score",
                            "params": {"combiner": true, "combiner.query": ["lex", "vec"], "combiner.algorithm": "rrf", "combiner.rrf.k": 60},
                        }),
                    ))
                }
                "hybrid (weighted)" => ranked(
                    json!("{!bool should=$lexw should=$vecw}"),
                    // Solr's BM25 leaves out the (k1 + 1) factor that Strata's and Elasticsearch's keep: 0.2 x 2.2.
                    json!({"lexw": "{!boost b=0.44 v=$lexq}", "vecw": "{!boost b=0.8 v=$vecq}", "lexq": lex, "vecq": knn(DEPTH)}),
                ),
                "boosted (bm25 × decay)" => ranked(
                    json!("{!boost b=$decay v=$lexq}"),
                    json!({"decay": format!("exp(mul({DECAY_RATE},abs(sub(year,2024))))"), "lexq": lex}),
                ),
                // Refined: without it, a facet over shards may count wrong (it did in 37 of 300 here).
                "facet" => json!({"query": format!("{{!term f=body}}{term}"), "limit": 0,
                    "facet": {"c": {"type": "terms", "field": "category", "limit": 5, "refine": true}}}),
                "key lookup" => json!({"query": format!("{{!term f=id}}{key}"), "limit": 10, "fields": "id,title,year,rating"}),
                _ => return None,
            };
            Some(("/solr/quora/query", body))
        }
    }
}

/// An answer reduced to what the engines share: the keys in order, their
/// scores, and a facet's (value, count) rows.
fn answer(engine: Engine, j: &Json) -> Json {
    let (hits, id, score) = match engine {
        Engine::Strata => {
            return json!({"keys": j["keys"], "scores": j["scores"], "rows": j["rows"]});
        }
        Engine::Es => (j["hits"]["hits"].as_array().cloned().unwrap_or_default(), "_id", "_score"),
        Engine::Solr => (j["response"]["docs"].as_array().cloned().unwrap_or_default(), "id", "score"),
    };
    let keys: Vec<Json> = hits.iter().map(|h| h[id].clone()).collect();
    let scores: Vec<Json> = hits.iter().map(|h| h[score].clone()).collect();
    let rows: Vec<Json> = match engine {
        Engine::Es => j["aggregations"]["c"]["buckets"].as_array().cloned().unwrap_or_default().iter().map(|b| json!([b["key"], b["doc_count"]])).collect(),
        _ => j["facets"]["c"]["buckets"].as_array().cloned().unwrap_or_default().iter().map(|b| json!([b["val"], b["count"]])).collect(),
    };
    json!({"keys": keys, "scores": scores, "rows": rows})
}

/// An error in a 2xx answer (Solr's combined handler, for one, can answer
/// 200 with an "error" object).
fn error_of(j: &Json) -> Option<String> {
    j.get("error").map(|e| e.to_string())
}

#[derive(Default)]
struct Timed {
    ms: Vec<f64>,
    errors: usize,
    first_error: Option<String>,
    seconds: f64,
}

struct Req {
    url: String,
    body: Vec<u8>,
}

/// Posts `reqs` round-robin from `concurrency` clients for `seconds`.
async fn drive(client: &reqwest::Client, reqs: Arc<Vec<Req>>, concurrency: usize, seconds: f64) -> Timed {
    let next = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let t0 = Instant::now();
    let tasks: Vec<_> = (0..concurrency)
        .map(|_| {
            let (client, reqs, next) = (client.clone(), reqs.clone(), next.clone());
            tokio::spawn(async move {
                let mut t = Timed::default();
                while Instant::now() < deadline {
                    let r = &reqs[next.fetch_add(1, Ordering::Relaxed) % reqs.len()];
                    let start = Instant::now();
                    let res = client.post(&r.url).header("content-type", "application/json").body(r.body.clone()).send().await;
                    let err = match res {
                        Ok(res) => {
                            let ok = res.status().is_success();
                            let j = res.json::<Json>().await.unwrap_or(Json::Null);
                            if ok {
                                error_of(&j)
                            } else {
                                Some(j.to_string())
                            }
                        }
                        Err(e) => Some(e.to_string()),
                    };
                    let ms = start.elapsed().as_secs_f64() * 1e3;
                    match err {
                        None => t.ms.push(ms),
                        Some(e) => {
                            t.errors += 1;
                            t.first_error.get_or_insert(e);
                        }
                    }
                }
                t
            })
        })
        .collect();
    let mut all = Timed::default();
    for task in tasks {
        let t = task.await.expect("client task");
        all.ms.extend(t.ms);
        all.errors += t.errors;
        if all.first_error.is_none() {
            all.first_error = t.first_error;
        }
    }
    all.seconds = t0.elapsed().as_secs_f64();
    all
}

#[allow(clippy::too_many_arguments)]
async fn run(collection: &str, engine: Engine, urls: &[String], n_queries: usize, concurrency: &[usize], seconds: f64, label: Option<String>, out: &Path) -> Result<(), String> {
    // As strata-server-bench: the first queries with text, and keys of the first 20,000 documents.
    let coll = data::load(collection, Some(20_000)).map_err(s)?;
    let queries: Vec<data::Query> = coll.queries.iter().filter(|q| !q.text.trim().is_empty()).take(n_queries).cloned().collect();
    let keys: Vec<String> = coll.docs.iter().map(|d| d[col::ID].as_str().unwrap_or("").to_string()).collect();
    let client = reqwest::Client::builder().pool_max_idle_per_host(1024).timeout(Duration::from_secs(30)).build().map_err(s)?;
    let label = label.unwrap_or_else(|| engine.name().to_string());
    let (mut http, mut answers, mut query_ids, mut unavailable, mut differ) = (Vec::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for &(kind, sql) in KINDS {
        let mut ids = Vec::new();
        let mut reqs = Vec::new();
        for (i, q) in queries.iter().enumerate() {
            if let Some((path, body)) = request(engine, kind, sql, q, &keys[(i * 7919) % keys.len()]) {
                ids.push(q.id.clone());
                reqs.push(Req { url: format!("{}{path}", urls[reqs.len() % urls.len()]), body: serde_json::to_vec(&body).unwrap() });
            }
        }
        if let Some(d) = differs(engine, kind) {
            differ.insert(kind, d);
        }
        // Every request once, from one client: the answers, and a warm-up.
        let mut got = Vec::new();
        let mut failed = None;
        for r in &reqs {
            let res = client.post(&r.url).header("content-type", "application/json").body(r.body.clone()).send().await;
            let (status, j) = match res {
                Ok(res) => (res.status(), res.json::<Json>().await.unwrap_or(Json::Null)),
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            };
            if let Some(e) = error_of(&j).or_else(|| (!status.is_success()).then(|| format!("{status}: {j}"))) {
                failed = Some(e);
                break;
            }
            got.push(answer(engine, &j));
        }
        if let Some(e) = failed {
            // The engine cannot answer this kind (it was checked to answer the others).
            eprintln!("{label:<8} {kind:<24} unavailable: {}", e.chars().take(300).collect::<String>());
            unavailable.insert(kind, e);
            continue;
        }
        answers.insert(kind, got);
        query_ids.insert(kind, ids);
        let reqs = Arc::new(reqs);
        for &c in concurrency {
            let t = drive(&client, reqs.clone(), c, seconds).await;
            eprintln!(
                "{label:<8} {kind:<24} c={c:<3} {:>8.0} q/s  p50 {:>7.3} ms  p99 {:>7.3} ms  errors {}",
                t.ms.len() as f64 / t.seconds,
                percentile(&t.ms, 50.0),
                percentile(&t.ms, 99.0),
                t.errors
            );
            if let Some(e) = &t.first_error {
                eprintln!("  first error: {}", e.chars().take(300).collect::<String>());
            }
            http.push(json!({
                "kind": kind, "concurrency": c, "requests": t.ms.len(), "qps": t.ms.len() as f64 / t.seconds.max(1e-9),
                "p50_ms": percentile(&t.ms, 50.0), "p95_ms": percentile(&t.ms, 95.0), "p99_ms": percentile(&t.ms, 99.0),
                "mean_ms": mean(&t.ms), "errors": t.errors, "first_error": t.first_error,
            }));
        }
    }
    let report = json!({
        "engine": engine.name(), "label": label, "collection": collection, "urls": urls, "queries": queries.len(),
        "params": {"seconds": seconds, "concurrency": concurrency, "candidates": CANDIDATES, "oversample": OVERSAMPLE, "hybrid_depth": DEPTH},
        "http": http, "unavailable": unavailable, "differs": differ, "query_ids": query_ids, "answers": answers,
    });
    std::fs::create_dir_all(out).map_err(s)?;
    let path = out.join(format!("run-{}.json", label.replace(' ', "-")));
    std::fs::write(&path, serde_json::to_string(&report).unwrap()).map_err(s)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let r = match &cli.cmd {
        Cmd::Build { shard, of, out, threads } => build(&cli.collection, *shard, *of, out, *threads),
        Cmd::Load { engine, url, shards, batch, clients, limit, out } => load(&cli.collection, *engine, url, *shards, *batch, *clients, *limit, out).await,
        Cmd::Run { engine, url, queries, concurrency, seconds, label, out } => run(&cli.collection, *engine, url, *queries, concurrency, *seconds, label.clone(), out).await,
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
