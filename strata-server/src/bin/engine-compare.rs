//! Strata, Elasticsearch, OpenSearch, Solr and Vespa on the same BEIR
//! collection, with the same shards and the same CPU and memory (see
//! `deploy/compare/README.md`):
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
//! fit in a pod's memory. `load` creates the index (Elasticsearch,
//! OpenSearch) or the collection and its schema (Solr) first, untimed (Vespa's
//! application package is deployed by `compare.sh`), then times the bulk
//! requests and the merge of every shard into one segment, as Strata's
//! databases are built (Vespa has no merge to ask for: its documents are
//! searchable as they are fed, and it compacts on its own). `run` sends each kind's requests once (recording
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
#[command(about = "Strata, Elasticsearch, OpenSearch, Solr and Vespa on the same BEIR collection")]
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
    /// Loads the collection into Elasticsearch, OpenSearch, Solr or Vespa over HTTP and merges each shard to one segment, timed.
    Load {
        #[arg(long, value_enum)]
        engine: Engine,
        /// The engine's nodes, comma-separated; bulk requests go round-robin.
        #[arg(long, value_delimiter = ',', required = true)]
        url: Vec<String>,
        #[arg(long, default_value_t = 2)]
        shards: usize,
        /// Documents per bulk request (Vespa: documents in flight per client, each its own request).
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
        /// Only these kinds, comma-separated (default: all).
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        /// Elasticsearch on a paid (or trial) licence: the weighted hybrid kind
        /// uses the linear retriever with min-max normalisation.
        #[arg(long)]
        paid: bool,
        #[arg(long)]
        out: PathBuf,
    },
    /// Times writes against an engine loaded with the collection: new
    /// documents one per request and in batches, an update of one field of
    /// existing documents, and how long a written document takes to be found.
    /// Then deletes what it wrote and puts the old values back (run it after `run`).
    Writes {
        #[arg(long, value_enum)]
        engine: Engine,
        /// The engine's HTTP endpoints, comma-separated (Strata: its coordinator); requests go round-robin.
        #[arg(long, value_delimiter = ',', required = true)]
        url: Vec<String>,
        #[arg(long, value_delimiter = ',', default_value = "1,4,16")]
        concurrency: Vec<usize>,
        #[arg(long, default_value_t = 6.0)]
        seconds: f64,
        /// Documents per batched write.
        #[arg(long, default_value_t = 100)]
        batch: usize,
        /// Writes timed until their document is found.
        #[arg(long, default_value_t = 30)]
        samples: usize,
        /// Names the run in the report (`--out/writes-<label>.json`).
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Streams new documents through a Kafka (Redpanda) topic into an
    /// engine: a producer at each of `--rates` documents a second, then a
    /// backlog as fast as it can, and a consumer that crashes halfway
    /// through a last stream, right after a write, and starts again. Times
    /// how long each document takes to be written, counts the events written
    /// twice and the documents lost, then deletes what it streamed (run it
    /// after `run`).
    Stream {
        #[arg(long, value_enum)]
        engine: Engine,
        /// The engine's HTTP endpoints, comma-separated (Strata: its coordinator); partitions go round-robin.
        #[arg(long, value_delimiter = ',', required = true)]
        url: Vec<String>,
        /// Kafka (or Redpanda) brokers, comma-separated.
        #[arg(long, default_value = "redpanda:9092")]
        brokers: String,
        #[arg(long, default_value_t = 4)]
        partitions: i32,
        /// Documents a second, each for `--seconds` (each carries a 1,024-number vector: about 10 KB of JSON).
        #[arg(long, value_delimiter = ',', default_value = "250,500,1000,2000")]
        rates: Vec<usize>,
        #[arg(long, default_value_t = 30.0)]
        seconds: f64,
        /// Documents produced as fast as the producer can, for the consumer's own top speed.
        #[arg(long, default_value_t = 20_000)]
        flat: usize,
        /// Documents of the crash test.
        #[arg(long, default_value_t = 10_000)]
        restart: usize,
        /// Most documents a write.
        #[arg(long, default_value_t = 500)]
        batch: usize,
        /// Names the run in the report (`--out/stream-<label>.json`).
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
    /// OpenSearch.
    Os,
    Solr,
    Vespa,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Engine::Strata => "strata",
            Engine::Es => "es",
            Engine::Os => "os",
            Engine::Solr => "solr",
            Engine::Vespa => "vespa",
        }
    }

    /// Elasticsearch and OpenSearch share their document, bulk and most
    /// search APIs.
    fn es_like(self) -> bool {
        matches!(self, Engine::Es | Engine::Os)
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

#[derive(Clone, Deserialize)]
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

/// OpenSearch's index: the same analysis and fields, with the vectors in
/// its k-NN plugin's on-disk mode at 4x compression, which is Lucene's HNSW
/// over 8-bit codes re-scored with the full vectors, as Elasticsearch's
/// `int8_hnsw` with `rescore_vector`.
fn os_index() -> Json {
    let mut b = es_index();
    b["settings"]["index.knn"] = json!(true);
    b["mappings"]["properties"]["emb"] = json!({
        "type": "knn_vector", "dimension": 1024, "space_type": "innerproduct", "mode": "on_disk", "compression_level": "4x",
        "method": {"name": "hnsw", "engine": "lucene", "parameters": {"m": 16, "ef_construction": 200}},
    });
    b
}

/// OpenSearch's hybrid queries normalise and combine in a search pipeline:
/// min-max scores weighted 0.2 and 0.8 (Strata's weighted kind), and RRF
/// with k 60.
fn os_pipelines() -> [(&'static str, Json); 2] {
    [
        (
            "minmax",
            json!({"phase_results_processors": [{"normalization-processor": {
                "normalization": {"technique": "min_max"},
                "combination": {"technique": "arithmetic_mean", "parameters": {"weights": [0.2, 0.8]}}}}]}),
        ),
        ("rrf", json!({"phase_results_processors": [{"score-ranker-processor": {"combination": {"technique": "rrf", "rank_constant": 60}}}]})),
    ]
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
        Engine::Es | Engine::Os => {
            let _ = client.delete(format!("{url}/quora")).send().await;
            let mut body = if engine == Engine::Es { es_index() } else { os_index() };
            body["settings"]["number_of_shards"] = json!(shards);
            check(client.put(format!("{url}/quora")).json(&body).send().await, "create index").await?;
            check(client.get(format!("{url}/_cluster/health/quora?wait_for_status=green&timeout=120s")).send().await, "index health").await?;
            if engine == Engine::Os {
                for (name, p) in os_pipelines() {
                    check(client.put(format!("{url}/_search/pipeline/{name}")).json(&p).send().await, "search pipeline").await?;
                }
            }
        }
        Engine::Vespa => {
            // The application package is deployed by compare.sh; drop what a previous run fed.
            check(client.delete(format!("{url}/document/v1/doc/doc/docid?selection=true&cluster=docs&timeout=600s")).send().await, "delete documents").await?;
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

/// A document id as one segment of a URL path (Vespa's /document/v1).
fn id_path(id: &str) -> String {
    let mut out = String::new();
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            write!(out, "%{b:02X}").unwrap();
        }
    }
    out
}

/// Posts one bulk request, again after a pause while the engine pushes back.
/// Vespa has no bulk request: its batch is one document a line (id, a tab,
/// the document), each put on its own, all of them at once.
async fn post_batch(engine: Engine, client: &reqwest::Client, url: &str, body: Arc<Vec<u8>>) -> Result<(), String> {
    if engine == Engine::Vespa {
        let text = std::str::from_utf8(&body).map_err(s)?;
        let puts = text.lines().map(|line| {
            let (id, doc) = line.split_once('\t').unwrap_or((line, "{}"));
            let url = format!("{url}/document/v1/doc/doc/docid/{}?timeout=180s", id_path(id));
            let (client, doc) = (client.clone(), doc.to_string());
            async move {
                let mut tries = 0;
                loop {
                    tries += 1;
                    let r = client.post(&url).header("content-type", "application/json").body(doc.clone()).send().await;
                    let retry = match &r {
                        Ok(r) => matches!(r.status().as_u16(), 429 | 503 | 504 | 507),
                        Err(e) => e.is_connect() || e.is_timeout(),
                    };
                    if retry && tries < 20 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    return check(r, "put").await.map(|_| ());
                }
            }
        });
        for r in futures::future::join_all(puts).await {
            r?;
        }
        return Ok(());
    }
    let (path, ctype) = match engine {
        Engine::Es | Engine::Os => ("/quora/_bulk", "application/x-ndjson"),
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
        if engine.es_like() && j["errors"].as_bool() == Some(true) {
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
                Engine::Vespa => {
                    body.extend_from_slice(r.id.as_bytes());
                    body.push(b'\t');
                    body.extend_from_slice(b"{\"fields\":");
                    body.extend_from_slice(doc_json(&r, &v).as_bytes());
                    body.extend_from_slice(b"}\n");
                }
                Engine::Es | Engine::Os => {
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
    // Vespa: a fed document is searchable once its put returns, and proton
    // flushes and compacts on its own (there is no merge to ask for).
    match engine {
        Engine::Es | Engine::Os => check(client.post(format!("{u}/quora/_refresh")).send().await, "refresh").await?,
        Engine::Vespa => Json::Null,
        _ => check(client.get(format!("{u}/solr/quora/update?commit=true")).send().await, "commit").await?,
    };
    let refresh_s = t1.elapsed().as_secs_f64();
    let t2 = Instant::now();
    match engine {
        Engine::Es | Engine::Os => check(client.post(format!("{u}/quora/_forcemerge?max_num_segments=1")).send().await, "force merge").await?,
        Engine::Vespa => Json::Null,
        _ => check(client.get(format!("{u}/solr/quora/update?optimize=true&maxSegments=1&waitSearcher=true")).send().await, "optimize").await?,
    };
    if engine.es_like() {
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
        Engine::Es | Engine::Os => {
            let st = check(client.get(format!("{u}/quora/_stats/store,docs,segments?level=shards")).send().await, "stats").await?;
            report["index_bytes"] = st["_all"]["primaries"]["store"]["size_in_bytes"].clone();
            report["indexed_docs"] = st["_all"]["primaries"]["docs"]["count"].clone();
            report["segments"] = st["_all"]["primaries"]["segments"]["count"].clone();
            let c = check(client.get(format!("{u}/quora/_count")).send().await, "count").await?;
            report["searchable_docs"] = c["count"].clone();
            // Elasticsearch only: the bytes of each field's structures.
            if engine == Engine::Es {
                let du = check(client.post(format!("{u}/quora/_disk_usage?run_expensive_tasks=true")).send().await, "disk usage").await?;
                report["disk_usage"] = du["quora"].clone();
            }
        }
        Engine::Vespa => {
            // The index's bytes on each content node are measured by compare.sh (du of proton's directory).
            let c = check(
                client.post(format!("{u}/search/")).json(&json!({"yql": "select * from doc where true", "hits": 0, "timeout": "30s"})).send().await,
                "count",
            )
            .await?;
            report["searchable_docs"] = c["root"]["fields"]["totalCount"].clone();
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

/// The kinds of `strata-server-bench`, then five more (phrase, typo,
/// prefix, similar items, SQL aggregate), as Strata SQL. `:seen` and `:y`
/// are written into the statement as literals (a list, and a number that
/// varies by query).
const KINDS: &[(&str, &str)] = &[
    ("bm25", "SELECT id, score() FROM docs ORDER BY bm25(body, :t, syntax => 'plain') DESC LIMIT 10"),
    ("bm25 + filter", "SELECT id, score() FROM docs WHERE year >= 2010 AND category IN ('c00', 'c01', 'c02', 'c03') ORDER BY bm25(body, :t, syntax => 'plain') DESC LIMIT 10"),
    ("vector", "SELECT id, score() FROM docs ORDER BY similarity(emb, :v) DESC LIMIT 10"),
    ("vector + filter", "SELECT id, score() FROM docs WHERE rating >= 4 ORDER BY similarity(emb, :v) DESC LIMIT 10"),
    ("hybrid (RRF)", "SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t, syntax => 'plain'), similarity(emb, :v)) DESC LIMIT 10"),
    ("hybrid (weighted)", "SELECT id, score() FROM docs ORDER BY 0.2 * minmax(bm25(body, :t, syntax => 'plain')) + 0.8 * minmax(similarity(emb, :v)) DESC LIMIT 10"),
    ("boosted (bm25 × decay)", "SELECT id, score() FROM docs ORDER BY bm25(body, :t, syntax => 'plain') * decay(year, 2024, 5) DESC LIMIT 10"),
    ("facet", "SELECT category, COUNT(*) FROM docs WHERE MATCH(body, :w) GROUP BY category ORDER BY 2 DESC LIMIT 5"),
    ("key lookup", "SELECT id, title, year, rating FROM docs WHERE id = :k"),
    // The rows with two neighbouring words of the query as a phrase, ranked by all of its words.
    ("phrase", "SELECT id, score() FROM docs ORDER BY bm25(body, :p) DESC LIMIT 10"),
    // The query with two letters of its longest word swapped, each word matching terms an edit or two away.
    ("typo", "SELECT id, score() FROM docs ORDER BY bm25(body, :x, syntax => 'plain', fuzzy => 'auto') DESC LIMIT 10"),
    // Search as you type: the rows with a word starting with the last word's first letters, ranked by the words before it.
    ("prefix", "SELECT id, score() FROM docs WHERE MATCH(body, :pre) ORDER BY bm25(body, :before, syntax => 'plain') DESC LIMIT 10"),
    // A recommender's request: the items nearest an item, rated 3 or more, minus the 20 the user has seen.
    ("similar items", "SELECT id, score() FROM docs WHERE rating >= 3 AND id NOT IN (:seen) ORDER BY similarity(emb, :i) DESC LIMIT 10"),
    // Analytics over the structured fields alone.
    ("sql aggregate", "SELECT category, COUNT(*), AVG(rating) FROM docs WHERE year >= :y GROUP BY category HAVING COUNT(*) > 1000 ORDER BY 2 DESC LIMIT 10"),
];

/// HNSW candidates for the vector kinds: Strata searches with ef 64 and
/// re-scores the best 40 with the full vectors; Elasticsearch and
/// OpenSearch get the same (num_candidates or ef_search 64, re-scoring 4 x
/// 10 with the full vectors); Solr's and Vespa's vectors are full precision,
/// searched with topK 64 (Vespa: target hits 10 plus 54 explored).
const CANDIDATES: usize = 64;
const OVERSAMPLE: usize = 4;
/// Each input's depth for the hybrid kinds (Strata's default).
const DEPTH: usize = 100;
/// ln(0.5) / 5: `decay(year, 2024, 5)`, exponential, halving every 5 years.
const DECAY_RATE: f64 = -0.13862943611198906;
/// Items a similar-items request leaves out (the item itself and 19 more).
const SEEN: usize = 20;

/// The longest ASCII word of `s` with at least 4 letters (as strata-server-bench picks it).
fn match_word(s: &str) -> Option<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 4).max_by_key(|w| w.len()).map(|w| w.to_lowercase())
}

/// The words of `s`, lowercased and space-separated: what every engine's
/// analysis splits it into anyway, without characters the Lucene query
/// parser would read as syntax (or upper-case AND, OR, NOT).
fn words(s: &str) -> String {
    let w = word_list(s);
    if w.is_empty() {
        "_".into()
    } else {
        w.join(" ")
    }
}

fn word_list(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

/// A word no engine's analysis drops: not a stopword of documents or of
/// ranked queries, at least 3 ASCII letters or digits.
fn kept(w: &str) -> bool {
    w.len() >= 3
        && w.bytes().all(|b| b.is_ascii_alphanumeric())
        && !strata::text::analyzer::STOPWORDS.contains(&w)
        && !strata::text::analyzer::QUERY_STOPWORDS.contains(&w)
}

/// The first two neighbouring words of the query that every engine keeps.
fn phrase_of(s: &str) -> Option<(String, String)> {
    let w = word_list(s);
    w.windows(2).find(|p| kept(&p[0]) && kept(&p[1])).map(|p| (p[0].clone(), p[1].clone()))
}

/// The query's words with the second and third letters of its longest
/// word (of 5 letters or more) swapped: one edit, as each engine counts a
/// transposition.
fn typo_of(s: &str) -> Option<String> {
    let mut w = word_list(s);
    let (i, _) = w.iter().enumerate().filter(|(_, x)| x.len() >= 5 && x.is_ascii()).max_by_key(|(i, x)| (x.len(), std::cmp::Reverse(*i)))?;
    let mut b = w[i].clone().into_bytes();
    if b[1] == b[2] {
        return None;
    }
    b.swap(1, 2);
    w[i] = String::from_utf8(b).ok()?;
    Some(w.join(" "))
}

/// Search as you type: the words before the last, and the last word's
/// first 3 letters (a query of at least two words whose last is kept).
fn prefix_of(s: &str) -> Option<(String, String)> {
    let w = word_list(s);
    let last = w.last()?;
    if w.len() < 2 || !kept(last) || last.len() < 4 {
        return None;
    }
    let before = w[..w.len() - 1].join(" ");
    if !w[..w.len() - 1].iter().any(|x| kept(x)) {
        return None;
    }
    Some((before, last[..3].to_string()))
}

/// The Lucene query parser's edits for a word in Strata's `'auto'` (and
/// Elasticsearch's `AUTO`): none up to 2 letters, 1 up to 5, else 2.
fn auto_edits(w: &str) -> usize {
    match w.chars().count() {
        0..=2 => 0,
        3..=5 => 1,
        _ => 2,
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

/// A string as a SQL literal.
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A string as a YQL literal.
fn yql_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Why an engine cannot answer a kind at all (nothing is sent).
fn unsupported(engine: Engine, kind: &str) -> Option<&'static str> {
    match (engine, kind) {
        (Engine::Vespa, "typo") => Some("Vespa matches typos (fuzzy) on attributes only, not on indexed text fields"),
        (Engine::Vespa, "prefix") => Some("Vespa matches prefixes on attributes only, not on indexed text fields (n-gram fields are its way)"),
        _ => None,
    }
}

/// What a kind means on an engine, where that is not what it means on Strata.
fn differs(engine: Engine, kind: &str, paid: bool) -> Option<&'static str> {
    match (engine, kind) {
        (Engine::Es, "hybrid (weighted)") if paid => {
            Some("the linear retriever (a paid feature, here on a trial licence): match and knn top 100, each min-max normalised over the top 100, weighted 0.2 and 0.8")
        }
        (Engine::Es, "hybrid (weighted)") => Some(
            "no min-max normalised fusion on the free licence (the linear retriever needs a paid one): the sum of 0.2 x the raw BM25 score and 0.8 x the vector score ((1 + dot) / 2) over the lexical matches and the knn top 100",
        ),
        (Engine::Solr, "hybrid (weighted)") => Some(
            "no min-max normalised fusion: the sum of 0.2 x the raw BM25 score (scaled by k1 + 1 = 2.2 as Elasticsearch's) and 0.8 x the vector score ((1 + dot) / 2) over the lexical matches and the knn top 100",
        ),
        (Engine::Es, "hybrid (RRF)") => Some("the rrf retriever"),
        (Engine::Os, "hybrid (RRF)") => Some("a hybrid query (match, knn k 100, pagination depth 100) through a search pipeline's score-ranker-processor, rrf, k 60"),
        (Engine::Os, "hybrid (weighted)") => {
            Some("a hybrid query (match, knn k 100, pagination depth 100) through a search pipeline's normalization-processor: min_max, arithmetic_mean weighted 0.2 and 0.8")
        }
        (Engine::Solr, "hybrid (RRF)") => Some("the combined query handler, rrf, k 60, knn topK 100; the lexical query's depth is the request's limit"),
        (Engine::Vespa, "hybrid (RRF)" | "hybrid (weighted)") => Some(
            "the union of the lexical matches and the nearestNeighbor top 100, ranked first by BM25 + closeness on each content node; the best 200 of those re-ranked in the global phase by reciprocal_rank_fusion (k 60) or 0.2 x normalize_linear(bm25) + 0.8 x normalize_linear(closeness)",
        ),
        (Engine::Vespa, "bm25" | "bm25 + filter" | "boosted (bm25 × decay)" | "phrase") => Some("weakAnd (Vespa's text query, which keeps the best 100 matches by a cheaper score, then ranks them by BM25)"),
        (Engine::Solr, "typo") => Some("each word~N with Strata's 'auto' edits; the Lucene parser does not stem a fuzzy word, so it is compared with the stemmed terms as typed"),
        (Engine::Vespa, "sql aggregate") => Some(
            "grouping (no SQL): the 10 largest categories of the filtered rows, those of 1,000 rows or fewer dropped by the client (grouping has no HAVING); precision 1,000, so that every content node sends all its groups and the counts are exact",
        ),
        (Engine::Vespa, "facet") => Some(
            "grouping over the rows whose body contains the word, precision 17 (each content node sends its 17 largest groups, as Elasticsearch's default shard_size of size x 1.5 + 10 does; at Vespa's default precision the counts were wrong in 11 of 60 SciFact facets)",
        ),
        _ => None,
    }
}

/// What one request of a kind is about: the query, and for the kinds that
/// need them a key, an item's vector and the items seen with it, and a year.
struct Probe<'a> {
    q: &'a data::Query,
    key: &'a str,
    item: &'a [f32],
    seen: &'a [String],
    year: i64,
    /// Elasticsearch on a paid (or trial) licence.
    paid: bool,
}

/// The request for one kind on one engine: the path under the engine's URL
/// and the JSON body (None: this query has nothing to ask of the kind, such
/// as a phrase in a query without two kept words in a row).
fn request(engine: Engine, kind: &str, sql: &str, p: &Probe) -> Option<(&'static str, Json)> {
    let t = &p.q.text;
    let key = p.key;
    // Facets need a word of the query (as strata-server-bench, they skip queries without one).
    let w = if sql.contains(":w") { match_word(t)? } else { String::new() };
    // MATCH(body, :w) analyses the word as documents are (no query
    // stopwords): the others look up that term as it is.
    let term = strata::text::analyzer::analyze(&w).into_iter().next().unwrap_or_else(|| w.clone());
    let v = &p.q.vector;
    let phrase = if kind == "phrase" { Some(phrase_of(t)?) } else { None };
    let typo = if kind == "typo" { Some(typo_of(t)?) } else { None };
    let prefix = if kind == "prefix" { Some(prefix_of(t)?) } else { None };
    let seen = p.seen;
    let year = p.year;
    match engine {
        Engine::Strata => {
            let mut params = serde_json::Map::new();
            if sql.contains(":t") {
                params.insert("t".into(), json!(t));
            }
            if sql.contains(":v") {
                params.insert("v".into(), json!(v.to_vec()));
            }
            if sql.contains(":w") {
                params.insert("w".into(), json!(&w));
            }
            if sql.contains(":k") {
                params.insert("k".into(), json!(key));
            }
            if let Some((a, b)) = &phrase {
                params.insert("p".into(), json!(format!("+\"{a} {b}\" {}", words(t))));
            }
            if let Some(x) = &typo {
                params.insert("x".into(), json!(x));
            }
            if let Some((before, pre)) = &prefix {
                params.insert("pre".into(), json!(format!("{pre}*")));
                params.insert("before".into(), json!(before));
            }
            if sql.contains(":i") {
                params.insert("i".into(), json!(p.item.to_vec()));
            }
            let sql = sql.replace(":seen", &seen.iter().map(|k| sql_str(k)).collect::<Vec<_>>().join(", ")).replace(":y", &year.to_string());
            Some(("/v1/query", json!({"sql": sql, "params": params})))
        }
        Engine::Es | Engine::Os => {
            let os = engine == Engine::Os;
            let m = json!({"match": {"body": t}});
            // Elasticsearch: knn with num_candidates and re-scoring; OpenSearch: knn with ef_search and re-scoring.
            let knn_query = |vec: &[f32], k: usize, n: usize, rescore: bool, filter: Option<Json>| {
                if os {
                    let mut q = json!({"vector": vec.to_vec(), "k": k, "method_parameters": {"ef_search": n.max(k)}});
                    if rescore {
                        q["rescore"] = json!({"oversample_factor": OVERSAMPLE});
                    }
                    if let Some(f) = filter {
                        q["filter"] = f;
                    }
                    json!({"knn": {"emb": q}})
                } else {
                    let mut q = json!({"field": "emb", "query_vector": vec.to_vec(), "k": k, "num_candidates": n});
                    if rescore {
                        q["rescore_vector"] = json!({"oversample": OVERSAMPLE});
                    }
                    if let Some(f) = filter {
                        q["filter"] = f;
                    }
                    q
                }
            };
            // Elasticsearch's knn is a top-level section; OpenSearch's a query.
            let knn_body = |q: Json| if os { json!({"query": q}) } else { json!({"knn": q}) };
            let top = |extra: Json| {
                let mut b = json!({"size": 10, "_source": false, "track_total_hits": false});
                b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                b
            };
            let index = "/quora/_search?request_cache=false";
            let body = match kind {
                "bm25" => top(json!({"query": m})),
                "bm25 + filter" => top(json!({"query": {"bool": {"must": [m], "filter": [
                    {"range": {"year": {"gte": 2010}}}, {"terms": {"category": ["c00", "c01", "c02", "c03"]}}]}}})),
                "vector" | "vector + filter" => {
                    let filter = (kind == "vector + filter").then(|| json!({"range": {"rating": {"gte": 4}}}));
                    top(knn_body(knn_query(v, 10, CANDIDATES, true, filter)))
                }
                "hybrid (RRF)" | "hybrid (weighted)" if os => {
                    let pipeline = if kind == "hybrid (RRF)" { "rrf" } else { "minmax" };
                    let body = json!({"size": 10, "_source": false, "track_total_hits": false, "query": {"hybrid": {
                        "queries": [m, knn_query(v, DEPTH, DEPTH, false, None)], "pagination_depth": DEPTH}}});
                    return Some((if pipeline == "rrf" { "/quora/_search?request_cache=false&search_pipeline=rrf" } else { "/quora/_search?request_cache=false&search_pipeline=minmax" }, body));
                }
                "hybrid (RRF)" => json!({"size": 10, "_source": false, "retriever": {"rrf": {
                    "retrievers": [{"standard": {"query": m}}, {"knn": knn_query(v, DEPTH, DEPTH, false, None)}],
                    "rank_window_size": DEPTH, "rank_constant": 60}}}),
                "hybrid (weighted)" if p.paid => json!({"size": 10, "_source": false, "retriever": {"linear": {
                    "retrievers": [
                        {"retriever": {"standard": {"query": m}}, "weight": 0.2, "normalizer": "minmax"},
                        {"retriever": {"knn": knn_query(v, DEPTH, DEPTH, false, None)}, "weight": 0.8, "normalizer": "minmax"},
                    ],
                    "rank_window_size": DEPTH}}}),
                "hybrid (weighted)" => {
                    let mut k = knn_query(v, DEPTH, DEPTH, false, None);
                    k["boost"] = json!(0.8);
                    top(json!({"query": {"match": {"body": {"query": t, "boost": 0.2}}}, "knn": k}))
                }
                "boosted (bm25 × decay)" => top(json!({"query": {"function_score": {"query": m,
                    "functions": [{"exp": {"year": {"origin": 2024, "scale": 5, "decay": 0.5}}}], "boost_mode": "multiply"}}})),
                "facet" => json!({"size": 0, "track_total_hits": false, "query": {"term": {"body": term}},
                    "aggs": {"c": {"terms": {"field": "category", "size": 5}}}}),
                "key lookup" => json!({"size": 10, "track_total_hits": false, "_source": ["title", "year", "rating"], "query": {"term": {"id": key}}}),
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    top(json!({"query": {"bool": {"must": [{"match_phrase": {"body": format!("{a} {b}")}}], "should": [m]}}}))
                }
                "typo" => top(json!({"query": {"match": {"body": {"query": typo.as_ref().unwrap(), "fuzziness": "AUTO"}}}})),
                "prefix" => {
                    let (before, pre) = prefix.as_ref().unwrap();
                    top(json!({"query": {"bool": {"filter": [{"prefix": {"body": pre}}], "must": [{"match": {"body": before}}]}}}))
                }
                "similar items" => {
                    let filter = json!({"bool": {"filter": [{"range": {"rating": {"gte": 3}}}], "must_not": [{"ids": {"values": seen}}]}});
                    top(knn_body(knn_query(p.item, 10, CANDIDATES, true, Some(filter))))
                }
                "sql aggregate" => {
                    // Elasticsearch SQL quotes identifiers (year is also a function); OpenSearch SQL reads a quoted one as a string.
                    let col = if os { "year" } else { "\"year\"" };
                    let q = format!(
                        "SELECT category, COUNT(*) AS n, AVG(rating) AS r FROM quora WHERE {col} >= {year} GROUP BY category HAVING COUNT(*) > 1000 ORDER BY n DESC LIMIT 10"
                    );
                    return Some(if os { ("/_plugins/_sql", json!({"query": q})) } else { ("/_sql?format=json", json!({"query": q})) });
                }
                _ => return None,
            };
            // The shard request cache would answer repeated size-0 requests (facets) from memory.
            Some((index, body))
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
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    ranked(json!(format!("{{!lucene df=body q.op=OR}}+\"{a} {b}\" {}", words(t))), json!({}))
                }
                "typo" => {
                    let fuzzy: Vec<String> =
                        word_list(typo.as_ref().unwrap()).iter().map(|w| match auto_edits(w) { 0 => w.clone(), n => format!("{w}~{n}") }).collect();
                    ranked(json!(format!("{{!lucene df=body q.op=OR}}{}", fuzzy.join(" "))), json!({}))
                }
                "prefix" => {
                    let (before, pre) = prefix.as_ref().unwrap();
                    let mut b = ranked(json!(format!("{{!lucene df=body q.op=OR}}{before}")), json!({}));
                    b["filter"] = json!([format!("body:{pre}*")]);
                    b
                }
                "similar items" => {
                    let ids: Vec<String> = seen.iter().map(|k| format!("\"{}\"", k.replace('"', "\\\""))).collect();
                    json!({"query": format!("{{!knn f=emb topK={CANDIDATES}}}{}", vector_text(p.item)), "limit": 10, "fields": "id,score",
                        "filter": ["rating:[3 TO *]", format!("-id:({})", ids.join(" OR "))]})
                }
                "sql aggregate" => {
                    let stmt = format!(
                        "SELECT category, COUNT(*) AS n, AVG(rating) AS r FROM quora WHERE \"year\" >= {year} GROUP BY category HAVING COUNT(*) > 1000 ORDER BY COUNT(*) DESC LIMIT 10"
                    );
                    return Some(("/solr/quora/sql", json!({"stmt": stmt})));
                }
                _ => return None,
            };
            Some(("/solr/quora/query", body))
        }
        Engine::Vespa => {
            let words_t = words(t);
            let text_query = |extra_where: &str| {
                json!({
                    "yql": format!("select * from doc where userQuery(){extra_where}"),
                    "query": words_t, "model.defaultIndex": "body", "model.type": "weakAnd",
                })
            };
            let nn = |field_v: &str, k: usize, extra: usize| format!("{{targetHits:{k}, hnsw.exploreAdditionalHits:{extra}}}nearestNeighbor(emb, {field_v})");
            let mut body = match kind {
                "bm25" => with(text_query(""), json!({"ranking": "bm25"})),
                "bm25 + filter" => with(text_query(" and year >= 2010 and category in (\"c00\", \"c01\", \"c02\", \"c03\")"), json!({"ranking": "bm25"})),
                "vector" | "vector + filter" => {
                    let filter = if kind == "vector + filter" { " and rating >= 4" } else { "" };
                    json!({"yql": format!("select * from doc where {}{filter}", nn("v", 10, CANDIDATES - 10)), "ranking": "vector", "input.query(v)": v.to_vec()})
                }
                "hybrid (RRF)" | "hybrid (weighted)" => json!({
                    "yql": format!("select * from doc where userQuery() or ({{targetHits:{DEPTH}}}nearestNeighbor(emb, v))"),
                    "query": words_t, "model.defaultIndex": "body", "model.type": "weakAnd",
                    "ranking": if kind == "hybrid (RRF)" { "hybrid_rrf" } else { "hybrid_weighted" }, "input.query(v)": v.to_vec(),
                }),
                "boosted (bm25 × decay)" => with(text_query(""), json!({"ranking": "boosted"})),
                "facet" => json!({
                    "yql": format!("select * from doc where body contains {} limit 0 | all(group(category) max(5) precision(17) order(-count()) each(output(count())))", yql_str(&w)),
                }),
                "key lookup" => json!({"yql": format!("select * from doc where id contains {}", yql_str(key)), "presentation.summary": "lookup", "ranking": "unranked"}),
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    with(text_query(&format!(" and body contains phrase({}, {})", yql_str(a), yql_str(b))), json!({"ranking": "bm25"}))
                }
                "similar items" => {
                    let ids: Vec<String> = seen.iter().map(|k| yql_str(k)).collect();
                    json!({"yql": format!("select * from doc where {} and rating >= 3 and !(id in ({}))", nn("i", 10, CANDIDATES - 10), ids.join(", ")),
                        "ranking": "similar", "input.query(i)": p.item.to_vec()})
                }
                "sql aggregate" => json!({
                    "yql": format!("select * from doc where year >= {year} limit 0 | all(group(category) max(10) precision(1000) order(-count()) each(output(count(), avg(rating))))"),
                }),
                _ => return None,
            };
            body["hits"] = json!(if matches!(kind, "facet" | "sql aggregate") { 0 } else { 10 });
            if body.get("presentation.summary").is_none() {
                body["presentation.summary"] = json!("minimal");
            }
            body["timeout"] = json!("10s");
            Some(("/search/", body))
        }
    }
}

/// `a` with the keys of `b` added.
fn with(mut a: Json, b: Json) -> Json {
    a.as_object_mut().unwrap().extend(b.as_object().cloned().unwrap_or_default());
    a
}

/// An answer reduced to what the engines share: the keys in order, their
/// scores, and an aggregate's rows (a facet's (value, count); the SQL
/// kind's (category, count, average rating)).
fn answer(engine: Engine, kind: &str, j: &Json) -> Json {
    let (hits, id, score) = match engine {
        Engine::Strata => {
            return json!({"keys": j["keys"], "scores": j["scores"], "rows": j["rows"]});
        }
        Engine::Es | Engine::Os if kind == "sql aggregate" => {
            // Elasticsearch: {"columns", "rows"}; OpenSearch: {"schema", "datarows"}.
            let rows = j.get("rows").or_else(|| j.get("datarows")).cloned().unwrap_or(json!([]));
            return json!({"keys": [], "scores": [], "rows": rows});
        }
        Engine::Solr if kind == "sql aggregate" => {
            let rows: Vec<Json> = j["result-set"]["docs"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter(|d| d.get("EOF").is_none())
                .map(|d| json!([d["category"], d["n"], d["r"]]))
                .collect();
            return json!({"keys": [], "scores": [], "rows": rows});
        }
        Engine::Vespa => {
            let children = j["root"]["children"].as_array().cloned().unwrap_or_default();
            let groups: Vec<Json> = children
                .iter()
                .filter(|c| c["id"].as_str().is_some_and(|i| i.starts_with("group:root")))
                .flat_map(|g| g["children"].as_array().cloned().unwrap_or_default())
                .flat_map(|list| list["children"].as_array().cloned().unwrap_or_default())
                .collect();
            if !groups.is_empty() || matches!(kind, "facet" | "sql aggregate") {
                let rows: Vec<Json> = groups
                    .iter()
                    .filter(|g| kind != "sql aggregate" || g["fields"]["count()"].as_u64().unwrap_or(0) > 1000)
                    .map(|g| {
                        if kind == "sql aggregate" {
                            json!([g["value"], g["fields"]["count()"], g["fields"]["avg(rating)"]])
                        } else {
                            json!([g["value"], g["fields"]["count()"]])
                        }
                    })
                    .collect();
                return json!({"keys": [], "scores": [], "rows": rows});
            }
            let keys: Vec<Json> = children.iter().map(|h| h["fields"]["id"].clone()).collect();
            let scores: Vec<Json> = children.iter().map(|h| h["relevance"].clone()).collect();
            return json!({"keys": keys, "scores": scores, "rows": []});
        }
        Engine::Es | Engine::Os => (j["hits"]["hits"].as_array().cloned().unwrap_or_default(), "_id", "_score"),
        Engine::Solr => (j["response"]["docs"].as_array().cloned().unwrap_or_default(), "id", "score"),
    };
    let keys: Vec<Json> = hits.iter().map(|h| h[id].clone()).collect();
    let scores: Vec<Json> = hits.iter().map(|h| h[score].clone()).collect();
    let rows: Vec<Json> = match engine {
        Engine::Es | Engine::Os => {
            j["aggregations"]["c"]["buckets"].as_array().cloned().unwrap_or_default().iter().map(|b| json!([b["key"], b["doc_count"]])).collect()
        }
        _ => j["facets"]["c"]["buckets"].as_array().cloned().unwrap_or_default().iter().map(|b| json!([b["val"], b["count"]])).collect(),
    };
    json!({"keys": keys, "scores": scores, "rows": rows})
}

/// An error in a 2xx answer (Solr's combined handler, for one, can answer
/// 200 with an "error" object).
fn error_of(j: &Json) -> Option<String> {
    // Vespa answers 200 with root.errors when, for one, a content node timed out.
    j.get("error").or_else(|| j["root"].get("errors")).map(|e| e.to_string())
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
async fn run(
    collection: &str,
    engine: Engine,
    urls: &[String],
    n_queries: usize,
    concurrency: &[usize],
    seconds: f64,
    label: Option<String>,
    only: &[String],
    paid: bool,
    out: &Path,
) -> Result<(), String> {
    // As strata-server-bench: the first queries with text, and keys of the first 20,000 documents.
    let coll = data::load(collection, Some(20_000)).map_err(s)?;
    let queries: Vec<data::Query> = coll.queries.iter().filter(|q| !q.text.trim().is_empty()).take(n_queries).cloned().collect();
    let keys: Vec<String> = coll.docs.iter().map(|d| d[col::ID].as_str().unwrap_or("").to_string()).collect();
    let items: Vec<Arc<[f32]>> = coll.docs.iter().map(|d| Arc::from(d[col::EMB].as_vector().unwrap_or(&[]))).collect();
    let client = reqwest::Client::builder().pool_max_idle_per_host(1024).timeout(Duration::from_secs(30)).build().map_err(s)?;
    let label = label.unwrap_or_else(|| engine.name().to_string());
    let (mut http, mut answers, mut query_ids, mut unavailable, mut differ) = (Vec::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    // What analyze.py needs to score the kinds that are not about a query's text: each request's item and seen keys, or year.
    let mut probes = BTreeMap::new();
    for &(kind, sql) in KINDS {
        if !only.is_empty() && !only.iter().any(|k| k == kind) {
            continue;
        }
        let mut ids = Vec::new();
        let mut reqs = Vec::new();
        let mut params = Vec::new();
        if let Some(why) = unsupported(engine, kind) {
            eprintln!("{label:<8} {kind:<24} unavailable: {why}");
            unavailable.insert(kind, why.to_string());
            continue;
        }
        for (i, q) in queries.iter().enumerate() {
            // The item and the keys it was seen with: documents of the first 20,000, picked as the key lookups' are.
            let k = (i * 7919) % keys.len();
            let it = (i * 104_729 + 13) % keys.len();
            let seen: Vec<String> = std::iter::once(it).chain((1..SEEN).map(|j| (it + j * 997) % keys.len())).map(|j| keys[j].clone()).collect();
            let probe = Probe { q, key: &keys[k], item: &items[it], seen: &seen, year: 1990 + (i % 30) as i64, paid };
            if let Some((path, body)) = request(engine, kind, sql, &probe) {
                ids.push(q.id.clone());
                match kind {
                    "similar items" => params.push(json!({"item": keys[it], "seen": seen})),
                    "sql aggregate" => params.push(json!({"year": probe.year})),
                    _ => {}
                }
                reqs.push(Req { url: format!("{}{path}", urls[reqs.len() % urls.len()]), body: serde_json::to_vec(&body).unwrap() });
            }
        }
        if reqs.is_empty() {
            eprintln!("{label:<8} {kind:<24} no query of the collection makes a request of this kind");
            unavailable.insert(kind, "no query makes a request of this kind".to_string());
            continue;
        }
        if let Some(d) = differs(engine, kind, paid) {
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
            got.push(answer(engine, kind, &j));
        }
        if let Some(e) = failed {
            // The engine cannot answer this kind (it was checked to answer the others).
            eprintln!("{label:<8} {kind:<24} unavailable: {}", e.chars().take(300).collect::<String>());
            unavailable.insert(kind, e);
            continue;
        }
        answers.insert(kind, got);
        query_ids.insert(kind, ids);
        if !params.is_empty() {
            probes.insert(kind, params);
        }
        let reqs = Arc::new(reqs);
        for &c in concurrency {
            let t = drive(&client, reqs.clone(), c, seconds).await;
            eprintln!(
                "{label:<8} {kind:<24} c={c:<3} {:>8.0} q/s  p50 {:>7.3} ms  p90 {:>7.3} ms  p99 {:>7.3} ms  errors {}",
                t.ms.len() as f64 / t.seconds,
                percentile(&t.ms, 50.0),
                percentile(&t.ms, 90.0),
                percentile(&t.ms, 99.0),
                t.errors
            );
            if let Some(e) = &t.first_error {
                eprintln!("  first error: {}", e.chars().take(300).collect::<String>());
            }
            http.push(json!({
                "kind": kind, "concurrency": c, "requests": t.ms.len(), "qps": t.ms.len() as f64 / t.seconds.max(1e-9),
                "p50_ms": percentile(&t.ms, 50.0), "p90_ms": percentile(&t.ms, 90.0), "p95_ms": percentile(&t.ms, 95.0),
                "p99_ms": percentile(&t.ms, 99.0), "p999_ms": percentile(&t.ms, 99.9), "max_ms": t.ms.iter().cloned().fold(0.0, f64::max),
                "mean_ms": mean(&t.ms), "errors": t.errors, "first_error": t.first_error,
            }));
        }
    }
    let report = json!({
        "engine": engine.name(), "label": label, "collection": collection, "urls": urls, "queries": queries.len(),
        "params": {"seconds": seconds, "concurrency": concurrency, "candidates": CANDIDATES, "oversample": OVERSAMPLE, "hybrid_depth": DEPTH, "paid": paid},
        "http": http, "unavailable": unavailable, "differs": differ, "query_ids": query_ids, "probes": probes, "answers": answers,
    });
    std::fs::create_dir_all(out).map_err(s)?;
    let path = out.join(format!("run-{}.json", label.replace(' ', "-")));
    std::fs::write(&path, serde_json::to_string(&report).unwrap()).map_err(s)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

/// One write: a request (PUT for Vespa's partial updates, POST otherwise),
/// or Vespa's batch, which is one request a document, all sent at once.
enum WriteReq {
    One { url: String, put: bool, body: Vec<u8> },
    Many(Vec<(String, Vec<u8>)>),
}

/// What a write's answer says went wrong, if anything.
async fn write_error(res: Result<reqwest::Response, reqwest::Error>) -> Option<String> {
    match res {
        Err(e) => Some(e.to_string()),
        Ok(res) => {
            let status = res.status();
            let j = res.json::<Json>().await.unwrap_or(Json::Null);
            if !status.is_success() {
                return Some(format!("{status}: {j}"));
            }
            if j["errors"].as_bool() == Some(true) {
                // A bulk request with an item that failed.
                let item = j["items"].as_array().and_then(|items| items.iter().find(|i| i.as_object().is_some_and(|o| o.values().any(|v| v.get("error").is_some()))).cloned());
                return Some(format!("bulk item failed: {}", item.unwrap_or(Json::Null)));
            }
            error_of(&j)
        }
    }
}

async fn send_write(client: &reqwest::Client, w: &WriteReq) -> Option<String> {
    match w {
        WriteReq::One { url, put, body } => {
            let ctype = if url.contains("/_bulk") { "application/x-ndjson" } else { "application/json" };
            let rb = if *put { client.put(url) } else { client.post(url) };
            write_error(rb.header("content-type", ctype).body(body.clone()).send().await).await
        }
        WriteReq::Many(reqs) => {
            // As loading does: each put again a second later while Vespa is overloaded.
            let all = reqs.iter().map(|(url, body)| async move { vespa_retry(|| client.post(url).header("content-type", "application/json").body(body.clone())).await });
            futures::future::join_all(all).await.into_iter().flatten().next()
        }
    }
}

/// Sends the writes `make(n)` for n = start, start + 1, ... from `concurrency`
/// clients for `seconds`; returns the timings and the next n.
async fn drive_writes(client: &reqwest::Client, make: Arc<dyn Fn(usize) -> WriteReq + Send + Sync>, start: usize, concurrency: usize, seconds: f64) -> (Timed, usize) {
    let next = Arc::new(AtomicUsize::new(start));
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let t0 = Instant::now();
    let tasks: Vec<_> = (0..concurrency)
        .map(|_| {
            let (client, make, next) = (client.clone(), make.clone(), next.clone());
            tokio::spawn(async move {
                let mut t = Timed::default();
                while Instant::now() < deadline {
                    let w = make(next.fetch_add(1, Ordering::Relaxed));
                    let start = Instant::now();
                    let err = send_write(&client, &w).await;
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
    (all, next.load(Ordering::Relaxed))
}

/// A document for Strata's /v1/write: its fields without `body`, which
/// Strata makes from the title and text.
fn strata_doc(r: &RawDoc, id: &str, v: &[f32]) -> Json {
    json!({"id": id, "title": r.title, "text": r.text, "year": r.year, "category": r.category, "rating": r.rating, "topic": r.topic, "emb": v})
}

/// The document `r` under the key `id`, with `rating`, as the engine's own JSON (the others' have `body`).
fn engine_doc(engine: Engine, r: &RawDoc, id: &str, rating: f64, v: &[f32]) -> String {
    let r = RawDoc { id: id.to_string(), rating, ..r.clone() };
    match engine {
        Engine::Strata => {
            let mut d = strata_doc(&r, id, v);
            d["rating"] = json!(rating);
            d.to_string()
        }
        _ => doc_json(&r, v),
    }
}

/// The document `r` written as a new one under the key `id`.
fn insert_one(engine: Engine, url: &str, r: &RawDoc, id: &str, v: &[f32]) -> WriteReq {
    let (url, body) = match engine {
        Engine::Strata => (format!("{url}/v1/write"), json!({"ops": [{"op": "insert", "doc": strata_doc(r, id, v)}]}).to_string()),
        Engine::Vespa => (format!("{url}/document/v1/doc/doc/docid/{}", id_path(id)), format!("{{\"fields\":{}}}", engine_doc(engine, r, id, r.rating, v))),
        Engine::Es | Engine::Os => (format!("{url}/quora/_doc/{}", id_path(id)), engine_doc(engine, r, id, r.rating, v)),
        Engine::Solr => (format!("{url}/solr/quora/update"), format!("[{}]", engine_doc(engine, r, id, r.rating, v))),
    };
    WriteReq::One { url, put: false, body: body.into_bytes() }
}

/// Writes of `docs` (documents a request) as new documents with keys `w<n>`, or (`update`) as a new rating of existing ones.
fn write_req(engine: Engine, url: &str, docs: &[(RawDoc, Vec<f32>)], first: usize, count: usize, update: bool) -> WriteReq {
    let pick = |n: usize| &docs[n % docs.len()];
    if count == 1 && !update {
        let (r, v) = pick(first);
        return insert_one(engine, url, r, &format!("w{first}"), v);
    }
    // An update picks the document as the query kinds pick their keys, and gives it a rating of 1.0 to 4.9.
    let upd = |n: usize| {
        let (r, v) = &docs[(n * 7919) % docs.len()];
        (r, v, 1.0 + (n % 40) as f64 / 10.0)
    };
    match engine {
        Engine::Strata => {
            let ops: Vec<Json> = (first..first + count)
                .map(|n| {
                    if update {
                        let (r, _, x) = upd(n);
                        json!({"op": "set", "key": r.id, "field": "rating", "value": x})
                    } else {
                        let (r, v) = pick(n);
                        json!({"op": "insert", "doc": strata_doc(r, &format!("w{n}"), v)})
                    }
                })
                .collect();
            WriteReq::One { url: format!("{url}/v1/write"), put: false, body: serde_json::to_vec(&json!({"ops": ops})).unwrap() }
        }
        Engine::Vespa => {
            let one = |n: usize| {
                if update {
                    // A partial update: the attribute is changed in memory, nothing else is written again.
                    let (r, _, x) = upd(n);
                    (format!("{url}/document/v1/doc/doc/docid/{}", id_path(&r.id)), json!({"fields": {"rating": {"assign": x}}}).to_string())
                } else {
                    let (r, v) = pick(n);
                    let id = format!("w{n}");
                    (format!("{url}/document/v1/doc/doc/docid/{id}"), format!("{{\"fields\":{}}}", engine_doc(engine, r, &id, r.rating, v)))
                }
            };
            if count == 1 {
                let (u, b) = one(first);
                WriteReq::One { url: u, put: update, body: b.into_bytes() }
            } else {
                assert!(!update, "Vespa's batched updates are not timed");
                WriteReq::Many((first..first + count).map(|n| { let (u, b) = one(n); (u, b.into_bytes()) }).collect())
            }
        }
        // An update sends the whole document again: their partial updates
        // rebuild a document from its stored source, which leaves out `body`
        // and `emb` here (as the index keeps them only in its index structures).
        Engine::Es | Engine::Os => {
            let doc = |n: usize| if update { let (r, v, x) = upd(n); (r.id.clone(), engine_doc(engine, r, &r.id, x, v)) } else { let (r, v) = pick(n); let id = format!("w{n}"); let d = engine_doc(engine, r, &id, r.rating, v); (id, d) };
            if count == 1 {
                let (id, d) = doc(first);
                WriteReq::One { url: format!("{url}/quora/_doc/{}", id_path(&id)), put: false, body: d.into_bytes() }
            } else {
                let mut body = Vec::new();
                for n in first..first + count {
                    let (id, d) = doc(n);
                    body.extend_from_slice(json!({"index": {"_id": id}}).to_string().as_bytes());
                    body.push(b'\n');
                    body.extend_from_slice(d.as_bytes());
                    body.push(b'\n');
                }
                WriteReq::One { url: format!("{url}/quora/_bulk"), put: false, body }
            }
        }
        Engine::Solr => {
            let docs: Vec<String> = (first..first + count)
                .map(|n| if update { let (r, v, x) = upd(n); engine_doc(engine, r, &r.id, x, v) } else { let (r, v) = pick(n); engine_doc(engine, r, &format!("w{n}"), r.rating, v) })
                .collect();
            WriteReq::One { url: format!("{url}/solr/quora/update"), put: false, body: format!("[{}]", docs.join(",")).into_bytes() }
        }
    }
}

/// Whether a search for the one word `w` finds a document.
async fn found(engine: Engine, client: &reqwest::Client, url: &str, w: &str) -> Result<bool, String> {
    let (u, body) = match engine {
        Engine::Strata => (format!("{url}/v1/query"), json!({"sql": "SELECT id FROM docs WHERE MATCH(body, :w) LIMIT 1", "params": {"w": w}})),
        Engine::Es | Engine::Os => (format!("{url}/quora/_search?request_cache=false"), json!({"size": 1, "_source": false, "query": {"match": {"body": w}}})),
        Engine::Solr => (format!("{url}/solr/quora/query"), json!({"query": format!("{{!term f=body}}{w}"), "limit": 1, "fields": "id"})),
        Engine::Vespa => (format!("{url}/search/"), json!({"yql": format!("select * from doc where body contains {}", yql_str(w)), "hits": 1, "presentation.summary": "minimal", "ranking": "unranked"})),
    };
    let j = check(client.post(u).json(&body).send().await, "search").await?;
    let n = match engine {
        Engine::Strata => j["keys"].as_array().map_or(0, |k| k.len()),
        Engine::Es | Engine::Os => j["hits"]["hits"].as_array().map_or(0, |h| h.len()),
        Engine::Solr => j["response"]["docs"].as_array().map_or(0, |d| d.len()),
        Engine::Vespa => j["root"]["children"].as_array().map_or(0, |c| c.len()),
    };
    Ok(n > 0)
}

/// A word no document has, made of letters the stemmers leave alone.
fn marker(i: usize) -> String {
    let mut w = String::from("zqx");
    let mut n = i;
    loop {
        w.push(b"bcdfghjk"[n % 8] as char);
        n /= 8;
        if n == 0 {
            break;
        }
    }
    w.push('q');
    w
}

#[allow(clippy::too_many_arguments)]
async fn writes(collection: &str, engine: Engine, urls: &[String], concurrency: &[usize], seconds: f64, batch: usize, samples: usize, label: Option<String>, out: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder().pool_max_idle_per_host(1024).timeout(Duration::from_secs(60)).build().map_err(s)?;
    let label = label.unwrap_or_else(|| engine.name().to_string());
    let u0 = &urls[0];
    // Searchable within a second of a write, as Elasticsearch's and
    // OpenSearch's default refresh is (loading turned it off); Solr's soft
    // commits the same. Strata and Vespa make a write searchable when it returns.
    let nrt = match engine {
        Engine::Es | Engine::Os => {
            check(client.put(format!("{u0}/quora/_settings")).json(&json!({"index": {"refresh_interval": "1s"}})).send().await, "settings").await?;
            "refresh_interval 1s (the default; loading turned it off)"
        }
        Engine::Solr => {
            check(client.post(format!("{u0}/solr/quora/config")).json(&json!({"set-property": {"updateHandler.autoSoftCommit.maxTime": 1000}})).send().await, "config").await?;
            "autoSoftCommit.maxTime 1000 ms (as Elasticsearch's 1 s refresh; loading turned it off)"
        }
        Engine::Strata | Engine::Vespa => "searchable when the write returns",
    };
    // The documents written: the first 20,000, each under a new key.
    let mut docs = Vec::new();
    each_doc(collection, Some(20_000), |r, v| {
        docs.push((r, v));
        Ok(())
    })?;
    let docs = Arc::new(docs);
    let urls = Arc::new(urls.to_vec());
    let mut rows = Vec::new();
    let (mut next_new, mut next_upd) = (0usize, 0usize);
    let phases: [(&str, usize, bool); 3] = [("insert one", 1, false), ("insert batch", batch, false), ("update one", 1, true)];
    for (phase, per, update) in phases {
        for &c in concurrency {
            let (docs2, urls2) = (docs.clone(), urls.clone());
            let make: Arc<dyn Fn(usize) -> WriteReq + Send + Sync> = Arc::new(move |n| write_req(engine, &urls2[n % urls2.len()], &docs2, n * per, per, update));
            let start = if update { next_upd } else { next_new.div_ceil(per) };
            let (t, end) = drive_writes(&client, make, start, c, seconds).await;
            if update {
                next_upd = end;
            } else {
                next_new = end * per;
            }
            let docs_s = (t.ms.len() * per) as f64 / t.seconds.max(1e-9);
            eprintln!(
                "{label:<8} {phase:<14} c={c:<3} {docs_s:>8.0} docs/s  p50 {:>8.3} ms  p90 {:>8.3} ms  p99 {:>8.3} ms  errors {}",
                percentile(&t.ms, 50.0),
                percentile(&t.ms, 90.0),
                percentile(&t.ms, 99.0),
                t.errors
            );
            if let Some(e) = &t.first_error {
                eprintln!("  first error: {}", e.chars().take(300).collect::<String>());
            }
            rows.push(json!({
                "phase": phase, "concurrency": c, "docs_per_request": per, "requests": t.ms.len(), "docs_per_s": docs_s,
                "p50_ms": percentile(&t.ms, 50.0), "p90_ms": percentile(&t.ms, 90.0), "p99_ms": percentile(&t.ms, 99.0),
                "max_ms": t.ms.iter().cloned().fold(0.0, f64::max), "mean_ms": mean(&t.ms), "errors": t.errors, "first_error": t.first_error,
            }));
        }
    }
    // Until visible: one write at a time of a document with a word of its
    // own, then a search for the word every 5 ms until it finds it.
    let (mut ack_ms, mut seen_ms) = (Vec::new(), Vec::new());
    for i in 0..samples {
        let (r, v) = &docs[i % docs.len()];
        let w = marker(i);
        let id = format!("v{i}");
        let doc = RawDoc { title: format!("{w} {}", r.title), ..r.clone() };
        let req = insert_one(engine, u0, &doc, &id, v);
        let t0 = Instant::now();
        if let Some(e) = send_write(&client, &req).await {
            return Err(format!("write: {e}"));
        }
        ack_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        loop {
            if found(engine, &client, u0, &w).await? {
                break;
            }
            if t0.elapsed() > Duration::from_secs(60) {
                return Err(format!("{id} not found a minute after it was written"));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        seen_ms.push(t0.elapsed().as_secs_f64() * 1e3);
    }
    eprintln!(
        "{label:<8} visible        written in p50 {:.1} ms, found after p50 {:.1} ms  p90 {:.1} ms  max {:.1} ms",
        percentile(&ack_ms, 50.0),
        percentile(&seen_ms, 50.0),
        percentile(&seen_ms, 90.0),
        seen_ms.iter().cloned().fold(0.0, f64::max)
    );
    let visible = json!({
        "samples": samples, "ack_p50_ms": percentile(&ack_ms, 50.0), "visible_p50_ms": percentile(&seen_ms, 50.0),
        "visible_p90_ms": percentile(&seen_ms, 90.0), "visible_max_ms": seen_ms.iter().cloned().fold(0.0, f64::max), "visible_ms": seen_ms,
    });

    // Put the index back: delete what was written, give the updated documents their ratings again.
    let t_clean = Instant::now();
    let new_keys: Vec<String> = (0..next_new).map(|n| format!("w{n}")).chain((0..samples).map(|i| format!("v{i}"))).collect();
    let mut touched: Vec<usize> = (0..next_upd).map(|n| (n * 7919) % docs.len()).collect();
    touched.sort_unstable();
    touched.dedup();
    restore(engine, &client, u0, &new_keys, &touched.iter().map(|&i| &docs[i]).collect::<Vec<_>>()).await?;
    eprintln!("{label:<8} deleted {} written documents and put back {} ratings in {:.1} s", new_keys.len(), touched.len(), t_clean.elapsed().as_secs_f64());

    let report = json!({
        "engine": engine.name(), "label": label, "collection": collection, "urls": urls.as_slice(),
        "params": {"seconds": seconds, "concurrency": concurrency, "batch": batch, "samples": samples, "searchable": nrt},
        "writes": rows, "visible": visible,
    });
    std::fs::create_dir_all(out).map_err(s)?;
    let path = out.join(format!("writes-{}.json", label.replace(' ', "-")));
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).map_err(s)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

/// Sends a request to Vespa again, a second later, while it is overloaded
/// (429 or 503) or the connection failed (its puts, updates and deletes are
/// idempotent).
async fn vespa_retry(rb: impl Fn() -> reqwest::RequestBuilder) -> Option<String> {
    let mut last = None;
    for _ in 0..30 {
        match rb().send().await {
            Ok(r) if matches!(r.status().as_u16(), 429 | 503) => last = Some(format!("{}", r.status())),
            Err(e) if !e.is_status() => last = Some(e.to_string()),
            res => return write_error(res).await,
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Some(format!("still failing after 30 tries: {}", last.unwrap_or_default()))
}

/// Deletes `keys` and writes `docs` again as they were (their ratings).
async fn restore(engine: Engine, client: &reqwest::Client, url: &str, keys: &[String], docs: &[&(RawDoc, Vec<f32>)]) -> Result<(), String> {
    // Vespa queues 256 requests at most.
    let size = if engine == Engine::Vespa { 64 } else { 500 };
    for chunk in keys.chunks(size) {
        let err = match engine {
            Engine::Strata => {
                let ops: Vec<Json> = chunk.iter().map(|k| json!({"op": "delete", "key": k})).collect();
                send_write(client, &WriteReq::One { url: format!("{url}/v1/write"), put: false, body: serde_json::to_vec(&json!({"ops": ops})).unwrap() }).await
            }
            Engine::Es | Engine::Os => {
                let body: String = chunk.iter().map(|k| format!("{}\n", json!({"delete": {"_id": k}}))).collect();
                send_write(client, &WriteReq::One { url: format!("{url}/quora/_bulk"), put: false, body: body.into_bytes() }).await
            }
            Engine::Solr => send_write(client, &WriteReq::One { url: format!("{url}/solr/quora/update"), put: false, body: json!({"delete": chunk}).to_string().into_bytes() }).await,
            Engine::Vespa => {
                let dels = chunk.iter().map(|k| async move { vespa_retry(|| client.delete(format!("{url}/document/v1/doc/doc/docid/{}", id_path(k)))).await });
                futures::future::join_all(dels).await.into_iter().flatten().next()
            }
        };
        if let Some(e) = err {
            return Err(format!("deleting the written documents: {e}"));
        }
    }
    for chunk in docs.chunks(size.min(200)) {
        let err = match engine {
            Engine::Strata => {
                let ops: Vec<Json> = chunk.iter().map(|(r, _)| json!({"op": "set", "key": r.id, "field": "rating", "value": r.rating})).collect();
                send_write(client, &WriteReq::One { url: format!("{url}/v1/write"), put: false, body: serde_json::to_vec(&json!({"ops": ops})).unwrap() }).await
            }
            Engine::Vespa => {
                let puts = chunk.iter().map(|(r, _)| async move {
                    let body = json!({"fields": {"rating": {"assign": r.rating}}}).to_string();
                    vespa_retry(|| client.put(format!("{url}/document/v1/doc/doc/docid/{}", id_path(&r.id))).header("content-type", "application/json").body(body.clone())).await
                });
                futures::future::join_all(puts).await.into_iter().flatten().next()
            }
            Engine::Es | Engine::Os => {
                let mut body = Vec::new();
                for (r, v) in chunk.iter().map(|d| (&d.0, &d.1)) {
                    body.extend_from_slice(json!({"index": {"_id": r.id}}).to_string().as_bytes());
                    body.push(b'\n');
                    body.extend_from_slice(doc_json(r, v).as_bytes());
                    body.push(b'\n');
                }
                send_write(client, &WriteReq::One { url: format!("{url}/quora/_bulk"), put: false, body }).await
            }
            Engine::Solr => {
                let docs: Vec<String> = chunk.iter().map(|(r, v)| doc_json(r, v)).collect();
                send_write(client, &WriteReq::One { url: format!("{url}/solr/quora/update"), put: false, body: format!("[{}]", docs.join(",")).into_bytes() }).await
            }
        };
        if let Some(e) = err {
            return Err(format!("putting the ratings back: {e}"));
        }
    }
    // Searchable again as after loading.
    match engine {
        Engine::Es | Engine::Os => {
            check(client.post(format!("{url}/quora/_refresh")).send().await, "refresh").await?;
        }
        Engine::Solr => {
            check(client.get(format!("{url}/solr/quora/update?commit=true")).send().await, "commit").await?;
        }
        _ => {}
    }
    Ok(())
}

/// Milliseconds since the epoch, now.
fn epoch_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1e3)
}

/// A request to an engine again, a second later, while it pushes back
/// (429, 503, a bulk item refused with 429) or cannot be reached: a
/// consumer drops no event. The answer's text.
async fn post_retry(client: &reqwest::Client, url: &str, ctype: &str, body: &[u8]) -> Result<String, String> {
    let mut last = String::new();
    for _ in 0..120 {
        match client.post(url).header("content-type", ctype).body(body.to_vec()).send().await {
            Ok(r) if matches!(r.status().as_u16(), 429 | 503) => last = r.status().to_string(),
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.map_err(s)?;
                if !status.is_success() {
                    return Err(format!("{status}: {}", text.chars().take(500).collect::<String>()));
                }
                // Elasticsearch's and OpenSearch's bulk answer 200 with the items that failed.
                if text.contains("\"errors\":true") {
                    if text.contains("\"status\":429") {
                        last = "bulk items refused with 429".into();
                    } else {
                        return Err(format!("bulk item failed: {}", text.chars().take(500).collect::<String>()));
                    }
                } else {
                    return Ok(text);
                }
            }
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(format!("still refused after 120 tries: {last}"))
}

/// One partition's progress, shared with the test (it outlives a consumer
/// that crashes).
#[derive(Default)]
struct PartState {
    /// The next offset to read when a consumer starts: Strata's, from the
    /// server; the others', the offset their consumer committed (every second).
    committed: i64,
    /// The last offset the engine acknowledged, if any.
    acked: Option<i64>,
    /// The time of the last commit.
    committed_at: Option<Instant>,
}

/// What the consumers of one phase share.
struct Phase {
    parts: Vec<std::sync::Mutex<PartState>>,
    /// Each written event's lag: from when it was due (its record's timestamp) to the engine's answer.
    lags: std::sync::Mutex<Vec<f64>>,
    /// The part of it before the engine: from when it was due to when the consumer had read it.
    fetch_lags: std::sync::Mutex<Vec<f64>>,
    written: AtomicUsize,
    /// Events Strata's server skipped as replays (its offsets were past them).
    skipped: AtomicUsize,
    stop: std::sync::atomic::AtomicBool,
    /// Crash (return without committing) once this many events were written.
    crash_at: Option<usize>,
}

impl Phase {
    fn new(parts: &[i64], crash_at: Option<usize>) -> Arc<Phase> {
        Arc::new(Phase {
            parts: parts.iter().map(|&o| std::sync::Mutex::new(PartState { committed: o, acked: None, committed_at: None })).collect(),
            lags: std::sync::Mutex::new(Vec::new()),
            fetch_lags: std::sync::Mutex::new(Vec::new()),
            written: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            stop: std::sync::atomic::AtomicBool::new(false),
            crash_at,
        })
    }
}

/// The next offset of every partition after a phase: what a consumer starting now reads first.
fn next_offsets(ph: &Phase) -> Vec<i64> {
    ph.parts.iter().map(|p| p.lock().unwrap().committed).collect()
}

/// Reads partition `p` from the phase's offset and writes its events to the
/// engine, at most `batch` a request, one request at a time, until told to
/// stop (or, with `crash_at`, until it crashes right after a write).
#[allow(clippy::too_many_arguments)]
async fn consume_part(
    engine: Engine,
    client: reqwest::Client,
    url: String,
    pc: Arc<rskafka::client::partition::PartitionClient>,
    topic: String,
    p: usize,
    ph: Arc<Phase>,
    batch: usize,
) -> Result<(), String> {
    let mut offset = ph.parts[p].lock().unwrap().committed;
    loop {
        if ph.stop.load(Ordering::Relaxed) {
            // Stopped between phases: everything written is committed.
            let mut st = ph.parts[p].lock().unwrap();
            st.committed = offset;
            return Ok(());
        }
        // A short wait: Redpanda answered a waiting fetch only at its
        // deadline, not when records came (100 ms put 140 ms under every lag).
        let (recs, _high) = pc.fetch_records(offset, 1..8_000_000, 10).await.map_err(|e| format!("fetch: {e}"))?;
        let recs: Vec<_> = recs.into_iter().filter(|r| r.offset >= offset).collect();
        let fetched = epoch_ms();
        ph.fetch_lags.lock().unwrap().extend(recs.iter().map(|r| fetched - r.record.timestamp.timestamp_millis() as f64));
        for chunk in recs.chunks(batch.max(1)) {
            let key = |r: &rskafka::record::RecordAndOffset| String::from_utf8_lossy(r.record.key.as_deref().unwrap_or_default()).into_owned();
            let value = |r: &rskafka::record::RecordAndOffset| r.record.value.clone().unwrap_or_default();
            let mut skipped = 0;
            match engine {
                Engine::Strata => {
                    // One stream of the chunk's events, each with its offset: the server keeps the offsets with the rows.
                    let mut body = Vec::new();
                    for r in chunk {
                        let src = json!({"name": format!("{topic}/{p}"), "offset": r.offset, "ts": r.record.timestamp.timestamp_millis()});
                        body.extend_from_slice(format!("{{\"source\":{src},\"op\":\"insert\",\"doc\":").as_bytes());
                        body.extend_from_slice(&value(r));
                        body.extend_from_slice(b"}\n");
                    }
                    let text = post_retry(&client, &format!("{url}/v1/stream"), "application/x-ndjson", &body).await?;
                    let done = text.lines().filter_map(|l| serde_json::from_str::<Json>(l).ok()).find(|j| j["done"].as_bool() == Some(true));
                    let done = done.ok_or_else(|| format!("stream answered without its end: {}", text.chars().take(300).collect::<String>()))?;
                    skipped = done["skipped"].as_u64().unwrap_or(0) as usize;
                }
                Engine::Es | Engine::Os => {
                    let mut body = Vec::new();
                    for r in chunk {
                        body.extend_from_slice(json!({"index": {"_id": key(r)}}).to_string().as_bytes());
                        body.push(b'\n');
                        body.extend_from_slice(&value(r));
                        body.push(b'\n');
                    }
                    post_retry(&client, &format!("{url}/quora/_bulk"), "application/x-ndjson", &body).await?;
                }
                Engine::Solr => {
                    let mut body = vec![b'['];
                    for (i, r) in chunk.iter().enumerate() {
                        if i > 0 {
                            body.push(b',');
                        }
                        body.extend_from_slice(&value(r));
                    }
                    body.push(b']');
                    post_retry(&client, &format!("{url}/solr/quora/update"), "application/json", &body).await?;
                }
                Engine::Vespa => {
                    // One put a document (Vespa's /document/v1), 48 at once a
                    // partition: four partitions stay within the 256 requests
                    // Vespa's container queues.
                    for sub in chunk.chunks(48) {
                        let puts = sub.iter().map(|r| {
                            let (client, u) = (client.clone(), format!("{url}/document/v1/doc/doc/docid/{}", id_path(&key(r))));
                            let mut body = b"{\"fields\":".to_vec();
                            body.extend_from_slice(&value(r));
                            body.push(b'}');
                            async move { post_retry(&client, &u, "application/json", &body).await }
                        });
                        for r in futures::future::join_all(puts).await {
                            r?;
                        }
                    }
                }
            }
            let now = epoch_ms();
            let last = chunk.last().unwrap().offset;
            ph.lags.lock().unwrap().extend(chunk.iter().map(|r| now - r.record.timestamp.timestamp_millis() as f64));
            ph.skipped.fetch_add(skipped, Ordering::Relaxed);
            let written = ph.written.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len();
            offset = last + 1;
            let mut st = ph.parts[p].lock().unwrap();
            st.acked = Some(last);
            if ph.crash_at.is_some_and(|n| written >= n) {
                // Crashed right after the engine took the write, before committing its offset.
                return Ok(());
            }
            // Strata's offset is committed with the rows; the others' consumer commits every second, as Kafka's do.
            if engine == Engine::Strata || st.committed_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                st.committed = offset;
                st.committed_at = Some(Instant::now());
            }
        }
        if recs.is_empty() && ph.crash_at.is_some_and(|n| ph.written.load(Ordering::Relaxed) >= n) {
            return Ok(());
        }
    }
}

/// Produces events `first .. first + count` (each a copy of a document under
/// the key `s<n>`), at `rate` a second (None: as fast as it can), round-robin
/// over the partitions, each record stamped with the time it was due.
/// Returns the seconds it took.
async fn produce_events(parts: &[Arc<rskafka::client::partition::PartitionClient>], values: &[(String, String)], first: usize, count: usize, rate: Option<f64>) -> Result<f64, String> {
    use rskafka::chrono::{TimeZone, Utc};
    let t0 = Instant::now();
    let e0 = epoch_ms();
    let record = |n: usize, due_ms: f64| {
        let (a, b) = &values[n % values.len()];
        let id = format!("s{n}");
        rskafka::record::Record {
            key: Some(id.clone().into_bytes()),
            value: Some(format!("{a}{id}{b}").into_bytes()),
            headers: BTreeMap::new(),
            timestamp: Utc.timestamp_millis_opt(due_ms as i64).unwrap(),
        }
    };
    let mut sent = 0;
    while sent < count {
        // Every 10 ms, the events due by now (flat out: 50 a partition).
        let due = match rate {
            Some(r) => ((t0.elapsed().as_secs_f64() * r) as usize).min(count),
            None => (sent + 50 * parts.len()).min(count),
        };
        if due > sent {
            let mut per: Vec<Vec<rskafka::record::Record>> = (0..parts.len()).map(|_| Vec::new()).collect();
            for i in sent..due {
                let due_ms = match rate {
                    Some(r) => e0 + i as f64 / r * 1e3,
                    None => epoch_ms(),
                };
                per[i % parts.len()].push(record(first + i, due_ms));
            }
            let sends = per.into_iter().zip(parts).filter(|(r, _)| !r.is_empty()).map(|(recs, pc)| async move {
                // Records of up to about 1 MB a request.
                let mut out = Vec::new();
                let mut it = recs.into_iter().peekable();
                while it.peek().is_some() {
                    let chunk: Vec<_> = it.by_ref().take(64).collect();
                    out.push(pc.produce(chunk, rskafka::client::partition::Compression::NoCompression).await.map(|_| ()));
                }
                out.into_iter().collect::<Result<(), _>>()
            });
            for r in futures::future::join_all(sends).await {
                r.map_err(|e| format!("produce: {e}"))?;
            }
            sent = due;
        }
        if rate.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    Ok(t0.elapsed().as_secs_f64())
}

/// How many documents the engine has (after making the latest writes searchable).
async fn count_docs(engine: Engine, client: &reqwest::Client, url: &str) -> Result<u64, String> {
    Ok(match engine {
        Engine::Strata => {
            let j = check(client.post(format!("{url}/v1/query")).json(&json!({"sql": "SELECT COUNT(*) FROM docs"})).send().await, "count").await?;
            j["rows"][0][0].as_u64().or_else(|| j["rows"][0].as_object().and_then(|o| o.values().next()).and_then(|v| v.as_u64())).unwrap_or(0)
        }
        Engine::Es | Engine::Os => {
            check(client.post(format!("{url}/quora/_refresh")).send().await, "refresh").await?;
            check(client.get(format!("{url}/quora/_count")).send().await, "count").await?["count"].as_u64().unwrap_or(0)
        }
        Engine::Solr => {
            check(client.get(format!("{url}/solr/quora/update?commit=true")).send().await, "commit").await?;
            check(client.get(format!("{url}/solr/quora/select?q=*:*&rows=0")).send().await, "count").await?["response"]["numFound"].as_u64().unwrap_or(0)
        }
        Engine::Vespa => {
            let j = check(client.post(format!("{url}/search/")).json(&json!({"yql": "select * from doc where true", "hits": 0, "timeout": "30s"})).send().await, "count").await?;
            j["root"]["fields"]["totalCount"].as_u64().unwrap_or(0)
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn stream(
    collection: &str,
    engine: Engine,
    urls: &[String],
    brokers: &str,
    partitions: i32,
    rates: &[usize],
    seconds: f64,
    flat: usize,
    restart: usize,
    batch: usize,
    label: Option<String>,
    out: &Path,
) -> Result<(), String> {
    use rskafka::client::{partition::UnknownTopicHandling, ClientBuilder};
    let label = label.unwrap_or_else(|| engine.name().to_string());
    let client = reqwest::Client::builder().pool_max_idle_per_host(1024).timeout(Duration::from_secs(120)).build().map_err(s)?;
    let u0 = urls[0].clone();
    // Searchable within a second of a write, as for `writes`.
    let searchable = match engine {
        Engine::Es | Engine::Os => {
            check(client.put(format!("{u0}/quora/_settings")).json(&json!({"index": {"refresh_interval": "1s"}})).send().await, "settings").await?;
            "written: searchable within the 1 s refresh"
        }
        Engine::Solr => {
            check(client.post(format!("{u0}/solr/quora/config")).json(&json!({"set-property": {"updateHandler.autoSoftCommit.maxTime": 1000}})).send().await, "config").await?;
            "written: searchable within the 1 s soft commit"
        }
        Engine::Strata => "searchable (the server answers a batch once its rows are visible)",
        Engine::Vespa => "searchable (a put is visible when it returns)",
    };
    // A topic of its own for the run.
    let kafka = ClientBuilder::new(brokers.split(',').map(str::to_string).collect()).max_message_size(64 << 20).build().await.map_err(|e| format!("kafka: {e}"))?;
    let topic = format!("docs-{}-{}", label.replace(' ', "-"), epoch_ms() as u64 / 1000);
    kafka.controller_client().map_err(|e| format!("kafka: {e}"))?.create_topic(&topic, partitions, 1, 30_000).await.map_err(|e| format!("create topic: {e}"))?;
    let mut parts = Vec::new();
    for p in 0..partitions {
        parts.push(Arc::new(kafka.partition_client(&topic, p, UnknownTopicHandling::Retry).await.map_err(|e| format!("partition {p}: {e}"))?));
    }
    // The events: the first 5,000 documents as JSON with their key cut out, under new keys.
    let mut values = Vec::new();
    each_doc(collection, Some(5_000), |r, v| {
        let d = doc_json(&RawDoc { id: "@@key@@".into(), ..r }, &v);
        let (a, b) = d.split_once("@@key@@").ok_or("no key in a document's JSON")?;
        values.push((a.to_string(), b.to_string()));
        Ok(())
    })?;
    let base = count_docs(engine, &client, &u0).await?;
    eprintln!("{label:<8} topic {topic}, {partitions} partitions; {base} documents before");

    let mut rows = Vec::new();
    let mut offsets = vec![0i64; partitions as usize];
    let mut produced = 0usize;
    // A phase: consumers from `offsets`, `count` events produced at `rate`, then until every event is written (or 5 minutes more).
    let run_phase = |name: String, rate: Option<f64>, count: usize, first: usize, offsets: Vec<i64>, crash_at: Option<usize>| {
        let (client, parts, values, topic, urls) = (client.clone(), parts.clone(), &values, topic.clone(), urls.to_vec());
        let label: &str = &label;
        async move {
            let ph = Phase::new(&offsets, crash_at);
            let consumers: Vec<_> = (0..parts.len())
                .map(|p| tokio::spawn(consume_part(engine, client.clone(), urls[p % urls.len()].clone(), parts[p].clone(), topic.clone(), p, ph.clone(), batch)))
                .collect();
            let t0 = Instant::now();
            let produce_s = produce_events(&parts, values, first, count, rate).await?;
            let target = crash_at.unwrap_or(count);
            let deadline = Instant::now() + Duration::from_secs(600);
            while ph.written.load(Ordering::Relaxed) < target && Instant::now() < deadline && !consumers.iter().all(|c| c.is_finished()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let all_s = t0.elapsed().as_secs_f64();
            ph.stop.store(true, Ordering::Relaxed);
            for c in consumers {
                c.await.map_err(s)??;
            }
            let lags = ph.lags.lock().unwrap().clone();
            let fetch_lags = ph.fetch_lags.lock().unwrap().clone();
            let written = ph.written.load(Ordering::Relaxed);
            eprintln!(
                "{label:<8} {name:<16} produced {count} in {produce_s:.1} s; written {written} in {all_s:.1} s ({:.0}/s); lag p50 {:.0} ms  p90 {:.0} ms  p99 {:.0} ms  max {:.0} ms (read p50 {:.0} ms)",
                written as f64 / all_s,
                percentile(&lags, 50.0),
                percentile(&lags, 90.0),
                percentile(&lags, 99.0),
                lags.iter().cloned().fold(0.0, f64::max),
                percentile(&fetch_lags, 50.0)
            );
            let row = json!({
                "phase": name, "rate": rate, "produced": count, "produce_s": produce_s, "written": written, "seconds": all_s,
                "written_per_s": written as f64 / all_s, "kept_up": written >= target && all_s <= produce_s + 2.0,
                "lag_p50_ms": percentile(&lags, 50.0), "lag_p90_ms": percentile(&lags, 90.0), "lag_p99_ms": percentile(&lags, 99.0),
                "lag_max_ms": lags.iter().cloned().fold(0.0, f64::max), "skipped": ph.skipped.load(Ordering::Relaxed),
                "fetch_lag_p50_ms": percentile(&fetch_lags, 50.0), "fetch_lag_p99_ms": percentile(&fetch_lags, 99.0),
            });
            Ok::<_, String>((row, ph))
        }
    };
    for &r in rates {
        let count = (r as f64 * seconds) as usize;
        let (row, ph) = run_phase(format!("{r}/s"), Some(r as f64), count, produced, offsets.clone(), None).await?;
        produced += count;
        offsets = next_offsets(&ph);
        let kept_up = row["kept_up"].as_bool() == Some(true);
        let drained = row["written"].as_u64() == Some(count as u64);
        rows.push(row);
        if !drained {
            return Err(format!("{r}/s: not every event was written within 10 minutes"));
        }
        if !kept_up {
            // Fell behind: the higher rates would only fall further behind.
            eprintln!("{label:<8} fell behind at {r}/s: no higher rate");
            break;
        }
    }
    let (row, ph) = run_phase("flat out".into(), None, flat, produced, offsets.clone(), None).await?;
    if row["written"].as_u64() != Some(flat as u64) {
        return Err("flat out: not every event was written within 10 minutes".into());
    }
    let flat_rate = row["written_per_s"].as_f64().unwrap_or(1000.0);
    produced += flat;
    offsets = next_offsets(&ph);
    rows.push(row);

    // A consumer that crashes halfway through, right after a write, and starts again.
    let rate = (flat_rate / 2.0).clamp(100.0, 5000.0);
    let (row, ph) = run_phase("before the crash".into(), Some(rate), restart, produced, offsets.clone(), Some(restart / 2)).await?;
    rows.push(row);
    let acked: Vec<Option<i64>> = ph.parts.iter().map(|p| p.lock().unwrap().acked).collect();
    let resume: Vec<i64> = match engine {
        // Strata's consumer starts after the offsets the server has.
        Engine::Strata => {
            let have = check(client.get(format!("{u0}/v1/sources")).send().await, "sources").await?;
            let list = have.as_array().cloned().or_else(|| have["sources"].as_array().cloned()).unwrap_or_default();
            (0..partitions as usize)
                .map(|p| list.iter().find(|s| s["name"].as_str() == Some(&format!("{topic}/{p}"))).and_then(|s| s["offset"].as_i64()).map_or(0, |o| o + 1))
                .collect()
        }
        _ => next_offsets(&ph),
    };
    let again: i64 = acked.iter().zip(&resume).map(|(a, r)| a.map_or(0, |a| (a + 1 - r).max(0))).sum();
    // The rest of the events, already produced: the new consumer reads them from where it resumes.
    let left_from = restart;
    let ph2 = Phase::new(&resume, None);
    let consumers: Vec<_> = (0..parts.len())
        .map(|p| tokio::spawn(consume_part(engine, client.clone(), urls[p % urls.len()].clone(), parts[p].clone(), topic.clone(), p, ph2.clone(), batch)))
        .collect();
    let t0 = Instant::now();
    let target = (left_from as i64 - ph.written.load(Ordering::Relaxed) as i64 + again).max(0) as usize;
    let deadline = Instant::now() + Duration::from_secs(300);
    while ph2.written.load(Ordering::Relaxed) < target && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A last look for stragglers.
    tokio::time::sleep(Duration::from_millis(500)).await;
    ph2.stop.store(true, Ordering::Relaxed);
    for c in consumers {
        c.await.map_err(s)??;
    }
    produced += restart;
    let after = count_docs(engine, &client, &u0).await?;
    let lost = (base + produced as u64) as i64 - after as i64;
    eprintln!(
        "{label:<8} after the crash   resumed in {:.1} s: {again} events written a second time, {} skipped by the server; {after} documents, {lost} lost",
        t0.elapsed().as_secs_f64(),
        ph2.skipped.load(Ordering::Relaxed)
    );
    let restart_row = json!({
        "events": restart, "crash_after": restart / 2, "rate": rate, "written_again": again,
        "skipped_by_server": ph2.skipped.load(Ordering::Relaxed), "resumed_written": ph2.written.load(Ordering::Relaxed),
        "documents_before": base, "documents_after": after, "expected": base + produced as u64, "lost": lost,
        "how": match engine {
            Engine::Strata => "the consumer resumes after the offsets the server keeps with the rows (GET /v1/sources)",
            _ => "the consumer resumes after the offsets it committed, every second, as Kafka's consumers do",
        },
    });

    // Delete what was streamed.
    let t_clean = Instant::now();
    let keys: Vec<String> = (0..produced).map(|n| format!("s{n}")).collect();
    restore(engine, &client, &u0, &keys, &[]).await?;
    eprintln!("{label:<8} deleted {} streamed documents in {:.1} s", keys.len(), t_clean.elapsed().as_secs_f64());
    let _ = kafka.controller_client().map_err(|e| e.to_string())?.delete_topic(&topic, 30_000).await;

    let report = json!({
        "engine": engine.name(), "label": label, "collection": collection, "urls": urls, "topic": topic, "partitions": partitions,
        "params": {"rates": rates, "seconds": seconds, "flat": flat, "restart": restart, "batch": batch, "written_means": searchable},
        "phases": rows, "restart": restart_row,
    });
    std::fs::create_dir_all(out).map_err(s)?;
    let path = out.join(format!("stream-{}.json", label.replace(' ', "-")));
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).map_err(s)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let r = match &cli.cmd {
        Cmd::Build { shard, of, out, threads } => build(&cli.collection, *shard, *of, out, *threads),
        Cmd::Load { engine, url, shards, batch, clients, limit, out } => load(&cli.collection, *engine, url, *shards, *batch, *clients, *limit, out).await,
        Cmd::Run { engine, url, queries, concurrency, seconds, label, kinds, paid, out } => {
            run(&cli.collection, *engine, url, *queries, concurrency, *seconds, label.clone(), kinds, *paid, out).await
        }
        Cmd::Writes { engine, url, concurrency, seconds, batch, samples, label, out } => {
            writes(&cli.collection, *engine, url, concurrency, *seconds, *batch, *samples, label.clone(), out).await
        }
        Cmd::Stream { engine, url, brokers, partitions, rates, seconds, flat, restart, batch, label, out } => {
            stream(&cli.collection, *engine, url, brokers, *partitions, rates, *seconds, *flat, *restart, *batch, label.clone(), out).await
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
