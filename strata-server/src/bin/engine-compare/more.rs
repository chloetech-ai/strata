//! Nine more engines on the same collection and the same query kinds:
//! Meilisearch, Typesense, Manticore Search, Qdrant, Weaviate, Milvus,
//! ParadeDB (Postgres with pg_search), Postgres on its own (its full-text
//! search and pgvector) and Redis (its query engine).
//!
//! Each runs as one node with the whole budget of the others' two
//! (`deploy/compare/README.md`). `load` creates the index (untimed), times
//! the bulk writes, then whatever the engine does before every document is
//! searchable and indexed (indexing tasks, index builds, merges), and
//! records what the engine says of its size; `compare.sh` measures the data
//! directory on disk. `run` times the kinds and records the answers as `run`
//! does for the others, in the same report. Each kind is the engine's
//! closest native request: `unavailable` and `differs` say where it has
//! none or where it means something else. `replay` sends them a schedule's
//! writes too ([`insert_req`], [`rating_req`], [`delete_reqs`]); `writes`
//! and streams are not measured for these engines yet.
//!
//! Postgres and Redis are spoken to over their own protocols (the simple
//! query protocol, with trust authentication, and RESP2), each in a few
//! lines here, so the harness needs no client library for them.

use super::*;
use reqwest::Method;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Other {
    Meili,
    Typesense,
    Manticore,
    Qdrant,
    Weaviate,
    Milvus,
    Paradedb,
    Postgres,
    Redis,
}

impl Other {
    pub fn name(self) -> &'static str {
        match self {
            Other::Meili => "meili",
            Other::Typesense => "typesense",
            Other::Manticore => "manticore",
            Other::Qdrant => "qdrant",
            Other::Weaviate => "weaviate",
            Other::Milvus => "milvus",
            Other::Paradedb => "paradedb",
            Other::Postgres => "postgres",
            Other::Redis => "redis",
        }
    }
}

/// The API key compare.sh starts Typesense with (it needs one).
const TYPESENSE_KEY: &str = "compare";
/// BM25's parameters, as every engine's default.
const K1: f64 = 1.2;
const B: f64 = 0.75;

// --- Talking to the engines ----------------------------------------------

/// One request: HTTP, a SQL statement (Postgres), or Redis commands sent
/// together (the answer is the last one's).
#[derive(Clone)]
pub enum Req {
    Http { method: Method, url: String, ctype: &'static str, body: Vec<u8> },
    Sql { url: String, sql: String },
    Redis { url: String, cmds: Vec<Vec<Vec<u8>>> },
}

impl Req {
    fn bytes(&self) -> usize {
        match self {
            Req::Http { body, .. } => body.len(),
            Req::Sql { sql, .. } => sql.len(),
            Req::Redis { cmds, .. } => cmds.iter().flatten().map(|a| a.len()).sum(),
        }
    }
}

fn post(url: String, body: &Json) -> Req {
    Req::Http { method: Method::POST, url, ctype: "application/json", body: serde_json::to_vec(body).unwrap() }
}

/// What went wrong: the connection (dropped, to be opened again) or the request.
enum Fault {
    Io(String),
    Answer(String),
}

fn io(e: impl std::fmt::Display) -> Fault {
    Fault::Io(e.to_string())
}

/// A client's connections: HTTP (pooled), and one Postgres or Redis
/// connection per URL, opened when first used.
pub(crate) struct Conns {
    other: Other,
    http: reqwest::Client,
    pg: HashMap<String, Pg>,
    redis: HashMap<String, Resp>,
}

impl Conns {
    pub(crate) fn new(other: Other, timeout: Duration) -> Result<Conns, String> {
        let mut h = reqwest::header::HeaderMap::new();
        if other == Other::Typesense {
            h.insert("x-typesense-api-key", reqwest::header::HeaderValue::from_static(TYPESENSE_KEY));
        }
        let http = reqwest::Client::builder().default_headers(h).pool_max_idle_per_host(1024).timeout(timeout).build().map_err(s)?;
        Ok(Conns { other, http, pg: HashMap::new(), redis: HashMap::new() })
    }

    pub(crate) fn fork(&self) -> Conns {
        Conns { other: self.other, http: self.http.clone(), pg: HashMap::new(), redis: HashMap::new() }
    }

    pub(crate) async fn exec(&mut self, r: &Req) -> Result<Json, String> {
        match r {
            Req::Http { method, url, ctype, body } => {
                let res = self.http.request(method.clone(), url).header("content-type", *ctype).body(body.clone()).send().await.map_err(s)?;
                let status = res.status();
                let text = res.text().await.map_err(s)?;
                let j = serde_json::from_str(&text).unwrap_or(Json::String(text));
                if !status.is_success() {
                    return Err(format!("{status}: {}", short(&j)));
                }
                fault(self.other, &j).map_or(Ok(j), Err)
            }
            Req::Sql { url, sql } => {
                if !self.pg.contains_key(url) {
                    let mut c = Pg::connect(url).await.map_err(|e| format!("connect {url}: {e}"))?;
                    if let Some(init) = pg_session(self.other) {
                        c.query(init).await.map_err(|e| match e {
                            Fault::Io(e) | Fault::Answer(e) => e,
                        })?;
                    }
                    self.pg.insert(url.clone(), c);
                }
                match self.pg.get_mut(url).unwrap().query(sql).await {
                    Ok(j) => Ok(j),
                    Err(Fault::Answer(e)) => Err(e),
                    Err(Fault::Io(e)) => {
                        self.pg.remove(url);
                        Err(e)
                    }
                }
            }
            Req::Redis { url, cmds } => {
                if !self.redis.contains_key(url) {
                    let c = Resp::connect(url).await.map_err(|e| format!("connect {url}: {e}"))?;
                    self.redis.insert(url.clone(), c);
                }
                match self.redis.get_mut(url).unwrap().run(cmds).await {
                    Ok(j) => Ok(j),
                    Err(Fault::Answer(e)) => Err(e),
                    Err(Fault::Io(e)) => {
                        self.redis.remove(url);
                        Err(e)
                    }
                }
            }
        }
    }

    async fn call(&mut self, method: Method, url: String, body: Option<Json>) -> Result<Json, String> {
        let body = body.map(|b| serde_json::to_vec(&b).unwrap()).unwrap_or_default();
        self.exec(&Req::Http { method, url, ctype: "application/json", body }).await
    }

    async fn sql(&mut self, url: &str, sql: &str) -> Result<Json, String> {
        match self.other {
            Other::Manticore => self.exec(&manticore_sql(url, sql)).await,
            _ => self.exec(&Req::Sql { url: url.to_string(), sql: sql.to_string() }).await,
        }
    }

    async fn redis(&mut self, url: &str, args: &[&str]) -> Result<Json, String> {
        self.exec(&Req::Redis { url: url.to_string(), cmds: vec![args.iter().map(|a| a.as_bytes().to_vec()).collect()] }).await
    }
}

/// At most 600 characters of an answer, for messages.
fn short(j: &Json) -> String {
    let t = match j {
        Json::String(t) => t.clone(),
        _ => j.to_string(),
    };
    t.chars().take(600).collect()
}

/// An error an engine reports in a 2xx answer.
fn fault(o: Other, j: &Json) -> Option<String> {
    match o {
        // multi_search answers 200 with each search's own error.
        Other::Typesense => {
            if let Some(r) = j["results"].as_array() {
                return r.iter().find_map(|x| x.get("error").map(|e| format!("{}: {e}", x["code"])));
            }
            // A document import: one line a document.
            match j {
                Json::String(t) => t.lines().find(|l| l.contains("\"success\":false")).map(|l| l.chars().take(600).collect()),
                _ if j["success"] == json!(false) => Some(short(j)),
                _ => None,
            }
        }
        Other::Weaviate => {
            if let Some(e) = j.get("errors").filter(|e| !e.is_null()) {
                return Some(short(e));
            }
            // A batch: each object's result.
            j.as_array().and_then(|items| items.iter().find_map(|i| i["result"].get("errors").filter(|e| !e.is_null()).map(short)))
        }
        Other::Milvus => j["code"].as_i64().filter(|&c| c != 0).map(|c| format!("{c}: {}", j["message"])),
        Other::Manticore => {
            if j["errors"] == json!(true) {
                let item = j["items"].as_array().and_then(|i| i.iter().find(|x| x.to_string().contains("\"error\"")).cloned());
                return Some(format!("bulk item failed: {}", short(&item.unwrap_or(Json::Null))));
            }
            let e = if j.is_array() { &j[0]["error"] } else { &j["error"] };
            e.as_str().filter(|e| !e.is_empty()).map(|e| e.to_string())
        }
        _ => None,
    }
}

/// Manticore's SQL over HTTP, its answers as JSON.
fn manticore_sql(url: &str, sql: &str) -> Req {
    let mut body = String::from("query=");
    for b in sql.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            body.push(b as char);
        } else {
            write!(body, "%{b:02X}").unwrap();
        }
    }
    Req::Http { method: Method::POST, url: format!("{url}/sql?mode=raw"), ctype: "application/x-www-form-urlencoded", body: body.into_bytes() }
}

/// Settings of each Postgres connection the clients open.
fn pg_session(o: Other) -> Option<&'static str> {
    match o {
        // HNSW searched with ef 64; filtered searches go on until they have
        // found the rows asked for (pgvector 0.8's iterative scans).
        Other::Postgres => Some("SET hnsw.ef_search = 64; SET hnsw.iterative_scan = strict_order"),
        _ => None,
    }
}

/// Postgres's simple query protocol, on a server that trusts its clients.
struct Pg {
    r: TokioBufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
}

impl Pg {
    /// `postgres://host:port[/database]`, as user postgres.
    async fn connect(url: &str) -> Result<Pg, String> {
        let rest = url.strip_prefix("postgres://").unwrap_or(url);
        let (host, db) = rest.split_once('/').unwrap_or((rest, "postgres"));
        let stream = TcpStream::connect(host).await.map_err(s)?;
        stream.set_nodelay(true).map_err(s)?;
        let (r, w) = stream.into_split();
        let mut pg = Pg { r: TokioBufReader::with_capacity(1 << 16, r), w };
        let mut body = 196_608u32.to_be_bytes().to_vec();
        for (k, v) in [("user", "postgres"), ("database", db)] {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut msg = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        msg.extend_from_slice(&body);
        pg.w.write_all(&msg).await.map_err(s)?;
        loop {
            let (t, m) = pg.read().await.map_err(|e| match e {
                Fault::Io(e) | Fault::Answer(e) => e,
            })?;
            match t {
                b'R' if m.len() >= 4 && m[..4] != [0, 0, 0, 0] => return Err("the server asks for a password: run it with trust authentication".into()),
                b'E' => return Err(pg_error(&m)),
                b'Z' => return Ok(pg),
                _ => {}
            }
        }
    }

    async fn read(&mut self) -> Result<(u8, Vec<u8>), Fault> {
        let t = self.r.read_u8().await.map_err(io)?;
        let len = self.r.read_u32().await.map_err(io)? as usize;
        let mut m = vec![0u8; len.saturating_sub(4)];
        self.r.read_exact(&mut m).await.map_err(io)?;
        Ok((t, m))
    }

    /// Runs `sql` (one statement or several): the last result's rows, each
    /// an array of its values (numbers as numbers, the rest as text).
    async fn query(&mut self, sql: &str) -> Result<Json, Fault> {
        let mut msg = vec![b'Q'];
        msg.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
        msg.extend_from_slice(sql.as_bytes());
        msg.push(0);
        self.w.write_all(&msg).await.map_err(io)?;
        let (mut types, mut rows, mut err) = (Vec::<u32>::new(), Vec::<Json>::new(), None);
        loop {
            let (t, m) = self.read().await?;
            match t {
                b'T' => {
                    types.clear();
                    rows.clear();
                    let n = u16::from_be_bytes([m[0], m[1]]) as usize;
                    let mut at = 2;
                    for _ in 0..n {
                        at += m[at..].iter().position(|&b| b == 0).unwrap_or(0) + 1;
                        types.push(u32::from_be_bytes(m[at + 6..at + 10].try_into().unwrap()));
                        at += 18;
                    }
                }
                b'D' => {
                    let n = u16::from_be_bytes([m[0], m[1]]) as usize;
                    let mut at = 2;
                    let mut row = Vec::with_capacity(n);
                    for i in 0..n {
                        let len = i32::from_be_bytes(m[at..at + 4].try_into().unwrap());
                        at += 4;
                        if len < 0 {
                            row.push(Json::Null);
                            continue;
                        }
                        let v = String::from_utf8_lossy(&m[at..at + len as usize]).into_owned();
                        at += len as usize;
                        row.push(match types.get(i) {
                            Some(20 | 21 | 23 | 26) => v.parse::<i64>().map(Json::from).unwrap_or(Json::String(v)),
                            Some(700 | 701 | 1700) => v.parse::<f64>().map(Json::from).unwrap_or(Json::String(v)),
                            Some(16) => Json::Bool(v == "t"),
                            _ => Json::String(v),
                        });
                    }
                    rows.push(Json::Array(row));
                }
                b'E' => err = Some(pg_error(&m)),
                b'Z' => break,
                _ => {}
            }
        }
        match err {
            Some(e) => Err(Fault::Answer(e)),
            None => Ok(json!({"rows": rows})),
        }
    }
}

/// An ErrorResponse's severity, code and message.
fn pg_error(m: &[u8]) -> String {
    let mut parts = BTreeMap::new();
    for f in m.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        parts.insert(f[0], String::from_utf8_lossy(&f[1..]).into_owned());
    }
    format!("{} {}: {}", parts.get(&b'S').cloned().unwrap_or_default(), parts.get(&b'C').cloned().unwrap_or_default(), parts.get(&b'M').cloned().unwrap_or_default())
}

/// Redis's RESP2: commands sent together, replies read as JSON.
struct Resp {
    r: TokioBufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
}

impl Resp {
    /// `redis://host:port`.
    async fn connect(url: &str) -> Result<Resp, String> {
        let host = url.strip_prefix("redis://").unwrap_or(url).trim_end_matches('/');
        let stream = TcpStream::connect(host).await.map_err(s)?;
        stream.set_nodelay(true).map_err(s)?;
        let (r, w) = stream.into_split();
        Ok(Resp { r: TokioBufReader::with_capacity(1 << 16, r), w })
    }

    async fn run(&mut self, cmds: &[Vec<Vec<u8>>]) -> Result<Json, Fault> {
        let mut buf = Vec::new();
        for c in cmds {
            buf.extend_from_slice(format!("*{}\r\n", c.len()).as_bytes());
            for a in c {
                buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
                buf.extend_from_slice(a);
                buf.extend_from_slice(b"\r\n");
            }
        }
        self.w.write_all(&buf).await.map_err(io)?;
        let (mut last, mut err) = (Json::Null, None);
        for _ in cmds {
            match self.reply().await? {
                Ok(v) => last = v,
                Err(e) => {
                    err.get_or_insert(e);
                }
            }
        }
        err.map_or(Ok(last), |e| Err(Fault::Answer(e)))
    }

    /// One reply (an error reply anywhere in it makes it an error).
    async fn reply(&mut self) -> Result<Result<Json, String>, Fault> {
        let mut stack: Vec<(usize, Vec<Json>)> = Vec::new();
        let mut first_err = None;
        loop {
            let mut line = Vec::new();
            self.r.read_until(b'\n', &mut line).await.map_err(io)?;
            if line.len() < 3 {
                return Err(Fault::Io("connection closed".into()));
            }
            let text = String::from_utf8_lossy(&line[1..line.len() - 2]).into_owned();
            let mut v = match line[0] {
                b'+' => Json::String(text),
                b'-' => {
                    first_err.get_or_insert(text);
                    Json::Null
                }
                b':' => text.parse::<i64>().map(Json::from).unwrap_or(Json::Null),
                b'$' => {
                    let n: i64 = text.parse().map_err(|_| Fault::Io(format!("bad reply {text}")))?;
                    if n < 0 {
                        Json::Null
                    } else {
                        let mut b = vec![0u8; n as usize + 2];
                        self.r.read_exact(&mut b).await.map_err(io)?;
                        b.truncate(n as usize);
                        Json::String(String::from_utf8_lossy(&b).into_owned())
                    }
                }
                b'*' => {
                    let n: i64 = text.parse().map_err(|_| Fault::Io(format!("bad reply {text}")))?;
                    if n > 0 {
                        stack.push((n as usize, Vec::new()));
                        continue;
                    }
                    if n == 0 {
                        Json::Array(vec![])
                    } else {
                        Json::Null
                    }
                }
                _ => return Err(Fault::Io(format!("unexpected reply {}", String::from_utf8_lossy(&line)))),
            };
            loop {
                let Some((left, items)) = stack.last_mut() else {
                    return Ok(first_err.map_or(Ok(v), Err));
                };
                items.push(v);
                *left -= 1;
                if *left > 0 {
                    break;
                }
                v = Json::Array(stack.pop().unwrap().1);
            }
        }
    }
}

// --- Loading ---------------------------------------------------------------

/// A term's dimension in Qdrant's sparse vectors.
fn term_id(t: &str) -> u32 {
    twox_hash::XxHash3_64::oneshot(t.as_bytes()) as u32
}

/// A document's BM25 term weights with Strata's analysis, for Qdrant (which
/// multiplies them by each term's IDF, `modifier: idf`): tf (k1 + 1) / (tf +
/// k1 (1 - b + b dl / avgdl)).
fn doc_sparse(body: &str, avgdl: f64) -> Json {
    let terms = strata::text::analyzer::analyze(body);
    let mut tf: BTreeMap<u32, f64> = BTreeMap::new();
    for t in &terms {
        *tf.entry(term_id(t)).or_default() += 1.0;
    }
    let norm = K1 * (1.0 - B + B * terms.len() as f64 / avgdl.max(1e-9));
    let (idx, val): (Vec<u32>, Vec<f64>) = tf.into_iter().map(|(i, f)| (i, f * (K1 + 1.0) / (f + norm))).unzip();
    json!({"indices": idx, "values": val})
}

/// A query's terms with Strata's analysis (query stopwords dropped), each weighing 1.
fn query_sparse(t: &str) -> Option<Json> {
    let mut idx: Vec<u32> = strata::text::analyzer::analyze_query(t).iter().map(|t| term_id(t)).collect();
    idx.sort_unstable();
    idx.dedup();
    if idx.is_empty() {
        return None;
    }
    let val = vec![1.0; idx.len()];
    Some(json!({"indices": idx, "values": val}))
}

/// The average number of terms of a document's body (BM25's avgdl), read
/// from docs.jsonl on four threads.
fn avg_len(collection: &str, limit: Option<usize>) -> Result<f64, String> {
    let path = data::data_dir().join(collection).join("docs.jsonl");
    let lines = BufReader::with_capacity(1 << 20, File::open(&path).map_err(s)?).lines().take(limit.unwrap_or(usize::MAX));
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<String>>(8);
    let rx = Arc::new(std::sync::Mutex::new(rx));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let rx = rx.clone();
            std::thread::spawn(move || -> Result<(u64, u64), String> {
                let (mut n, mut len) = (0u64, 0u64);
                loop {
                    let Ok(chunk) = rx.lock().unwrap().recv() else { return Ok((n, len)) };
                    for l in chunk {
                        let r: RawDoc = serde_json::from_str(&l).map_err(s)?;
                        len += strata::text::analyzer::analyze(&r.body()).len() as u64;
                        n += 1;
                    }
                }
            })
        })
        .collect();
    let mut chunk = Vec::new();
    for l in lines {
        chunk.push(l.map_err(s)?);
        if chunk.len() == 2000 {
            tx.send(std::mem::take(&mut chunk)).map_err(s)?;
        }
    }
    if !chunk.is_empty() {
        tx.send(chunk).map_err(s)?;
    }
    drop(tx);
    let (mut n, mut len) = (0u64, 0u64);
    for w in workers {
        let (a, b) = w.join().map_err(|_| "avgdl thread panicked".to_string())??;
        n += a;
        len += b;
    }
    Ok(len as f64 / n.max(1) as f64)
}

/// An object's JSON with one more member, written as given (vectors are
/// written with their shortest decimals, as `doc_json` does).
fn plus(obj: &Json, name: &str, raw: &str) -> String {
    let mut out = serde_json::to_string(obj).unwrap();
    out.pop();
    write!(out, ",\"{name}\":{raw}}}").unwrap();
    out
}

/// A key as a Meilisearch document id, which takes only letters, digits,
/// `-` and `_` (MS MARCO's keys have `.` and `#`): `_` becomes `__` and any
/// other byte `_` and two hex digits, so that [`meili_key`] gives it back.
fn meili_id(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        match b {
            b'_' => out.push_str("__"),
            b if b.is_ascii_alphanumeric() || b == b'-' => out.push(b as char),
            b => out.push_str(&format!("_{b:02x}")),
        }
    }
    out
}

/// The key a [`meili_id`] came from.
fn meili_key(id: &str) -> String {
    let b = id.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'_' && i + 1 < b.len() && b[i + 1] == b'_' {
            out.push(b'_');
            i += 2;
        } else if b[i] == b'_' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&id[i + 1..i + 3], 16).unwrap_or(b'?'));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A 64-bit hash of a key as a UUID (Weaviate's object ids).
fn uuid_of(key: &str) -> String {
    let a = twox_hash::XxHash3_64::oneshot_with_seed(0, key.as_bytes());
    let b = twox_hash::XxHash3_64::oneshot_with_seed(1, key.as_bytes());
    // Version 8 (custom), RFC variant.
    let a = (a & !0xF000) | 0x8000;
    let b = (b & !(0b11 << 62)) | (0b10 << 62);
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xFFFF, a & 0xFFFF, b >> 48, b & 0xFFFF_FFFF_FFFF)
}

/// A vector as Postgres's text (`'[...]'`) and Manticore's (`(...)`).
fn pg_vec(v: &[f32]) -> String {
    sql_str(&vector_text(v))
}

fn manticore_vec(v: &[f32]) -> String {
    let t = vector_text(v);
    format!("({})", &t[1..t.len() - 1])
}

/// Documents as the engine's bulk request; `ids(i, doc)` numbers the i-th
/// for Qdrant and Manticore (a loaded document's row; [`new_id`] for one a
/// replay inserts).
fn batch_req(o: Other, url: &str, rows: &[(RawDoc, Vec<f32>)], ids: &dyn Fn(usize, &RawDoc) -> u64, avgdl: f64) -> Req {
    let fields = |r: &RawDoc| json!({"title": r.title, "body": r.body(), "year": r.year, "category": r.category, "rating": r.rating, "topic": r.topic});
    let lines = |f: &dyn Fn(u64, &RawDoc, &[f32]) -> String, sep: &str| -> Vec<u8> {
        let mut out = Vec::new();
        for (i, (r, v)) in rows.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(sep.as_bytes());
            }
            out.extend_from_slice(f(ids(i, r), r, v).as_bytes());
        }
        out
    };
    match o {
        Other::Meili => {
            let mut body = b"[".to_vec();
            body.extend(lines(
                &|_, r, v| {
                    let mut d = fields(r);
                    d["id"] = json!(meili_id(&r.id));
                    plus(&d, "_vectors", &format!("{{\"default\":{}}}", vector_text(v)))
                },
                ",",
            ));
            body.push(b']');
            Req::Http { method: Method::POST, url: format!("{url}/indexes/docs/documents?primaryKey=id"), ctype: "application/json", body }
        }
        Other::Typesense => {
            let body = lines(
                &|_, r, v| {
                    let mut d = fields(r);
                    d["id"] = json!(r.id);
                    plus(&d, "emb", &vector_text(v))
                },
                "\n",
            );
            Req::Http { method: Method::POST, url: format!("{url}/collections/docs/documents/import?action=upsert"), ctype: "text/plain", body }
        }
        Other::Qdrant => {
            let mut body = b"{\"points\":[".to_vec();
            body.extend(lines(
                &|n, r, v| {
                    let mut p = fields(r);
                    p["key"] = json!(r.id);
                    let vectors = format!("{{\"emb\":{},\"bm25\":{}}}", vector_text(v), doc_sparse(&r.body(), avgdl));
                    plus(&json!({"id": n, "payload": p}), "vector", &vectors)
                },
                ",",
            ));
            body.extend_from_slice(b"]}");
            Req::Http { method: Method::PUT, url: format!("{url}/collections/docs/points?wait=true"), ctype: "application/json", body }
        }
        Other::Weaviate => {
            let mut body = b"{\"objects\":[".to_vec();
            body.extend(lines(
                &|_, r, v| {
                    let mut p = fields(r);
                    p["key"] = json!(r.id);
                    plus(&json!({"class": "Doc", "id": uuid_of(&r.id), "properties": p}), "vector", &vector_text(v))
                },
                ",",
            ));
            body.extend_from_slice(b"]}");
            Req::Http { method: Method::POST, url: format!("{url}/v1/batch/objects"), ctype: "application/json", body }
        }
        Other::Milvus => {
            let mut body = b"{\"collectionName\":\"docs\",\"data\":[".to_vec();
            body.extend(lines(
                &|_, r, v| {
                    let mut d = fields(r);
                    d["key"] = json!(r.id);
                    // VarChar's limit, in bytes (MS MARCO has titles of 45 KB).
                    let cut = |mut t: String| {
                        while t.len() > 65_535 {
                            t.pop();
                        }
                        t
                    };
                    d["title"] = json!(cut(r.title.clone()));
                    d["body"] = json!(cut(r.body()));
                    plus(&d, "emb", &vector_text(v))
                },
                ",",
            ));
            body.extend_from_slice(b"]}");
            Req::Http { method: Method::POST, url: format!("{url}/v2/vectordb/entities/insert"), ctype: "application/json", body }
        }
        Other::Manticore => {
            let mut body = lines(
                &|n, r, v| {
                    let mut d = fields(r);
                    d["key"] = json!(r.id);
                    let doc = plus(&d, "emb", &vector_text(v));
                    format!("{{\"replace\":{{\"table\":\"docs\",\"id\":{},\"doc\":{doc}}}}}", n + 1)
                },
                "\n",
            );
            body.push(b'\n');
            Req::Http { method: Method::POST, url: format!("{url}/bulk"), ctype: "application/x-ndjson", body }
        }
        Other::Paradedb | Other::Postgres => {
            let mut sql = String::from("INSERT INTO docs (key, title, body, year, category, rating, topic, emb) VALUES ");
            for (i, (r, v)) in rows.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                write!(
                    sql,
                    "({}, {}, {}, {}, {}, {}, {}, {})",
                    sql_str(&r.id),
                    // Postgres text has no NUL (6 of MS MARCO's first 1M passages have one).
                    sql_str(&r.title.replace('\0', " ")),
                    sql_str(&r.body().replace('\0', " ")),
                    r.year,
                    sql_str(&r.category),
                    r.rating,
                    sql_str(&r.topic),
                    pg_vec(v)
                )
                .unwrap();
            }
            Req::Sql { url: url.to_string(), sql }
        }
        Other::Redis => {
            let cmds = rows
                .iter()
                .map(|(r, v)| {
                    let blob = f16_bytes(v);
                    let mut c: Vec<Vec<u8>> = vec![b"HSET".to_vec(), format!("d:{}", r.id).into_bytes()];
                    for (k, val) in [
                        ("key", r.id.clone()),
                        ("title", r.title.clone()),
                        ("body", r.body()),
                        ("year", r.year.to_string()),
                        ("category", r.category.clone()),
                        ("rating", r.rating.to_string()),
                        ("topic", r.topic.clone()),
                    ] {
                        c.push(k.as_bytes().to_vec());
                        c.push(val.into_bytes());
                    }
                    c.push(b"emb".to_vec());
                    c.push(blob);
                    c
                })
                .collect();
            Req::Redis { url: url.to_string(), cmds }
        }
    }
}

/// Typesense's stopwords for ranked queries: the documents' and the queries' (as Strata drops).
fn all_stopwords() -> Vec<&'static str> {
    let mut w: Vec<&str> = CLASSIC_STOPWORDS.to_vec();
    w.extend(strata::text::analyzer::QUERY_STOPWORDS.iter().copied());
    w.sort_unstable();
    w.dedup();
    w
}

/// Creates the empty index, dropping one a previous run left.
async fn setup(o: Other, c: &mut Conns, u: &str, dim: usize) -> Result<(), String> {
    match o {
        Other::Meili => {
            // Fails (as a task) when there is no index to delete.
            if let Ok(t) = c.call(Method::DELETE, format!("{u}/indexes/docs"), None).await {
                let _ = meili_wait(c, u, &t).await;
            }
            let t = c.call(Method::POST, format!("{u}/indexes"), Some(json!({"uid": "docs", "primaryKey": "id"}))).await?;
            meili_wait(c, u, &t).await?;
            let settings = json!({
                "searchableAttributes": ["body"],
                "displayedAttributes": ["id", "title", "year", "rating"],
                "filterableAttributes": ["id", "year", "category", "rating"],
                "stopWords": CLASSIC_STOPWORDS,
                "embedders": {"default": {"source": "userProvided", "dimensions": dim}},
                "faceting": {"maxValuesPerFacet": 100},
            });
            let t = c.call(Method::PATCH, format!("{u}/indexes/docs/settings"), Some(settings)).await?;
            meili_wait(c, u, &t).await?;
        }
        Other::Typesense => {
            let _ = c.call(Method::DELETE, format!("{u}/collections/docs"), None).await;
            c.call(Method::PUT, format!("{u}/stopwords/query"), Some(json!({"stopwords": all_stopwords(), "locale": "en"}))).await?;
            c.call(
                Method::POST,
                format!("{u}/collections"),
                Some(json!({"name": "docs", "fields": [
                    {"name": "title", "type": "string", "index": false, "optional": true},
                    {"name": "body", "type": "string", "stem": true},
                    {"name": "year", "type": "int32"},
                    {"name": "category", "type": "string", "facet": true},
                    {"name": "rating", "type": "float"},
                    {"name": "topic", "type": "string", "facet": true},
                    {"name": "emb", "type": "float[]", "num_dim": dim, "vec_dist": "ip", "hnsw_params": {"M": 16, "ef_construction": 200}},
                ]})),
            )
            .await?;
        }
        Other::Qdrant => {
            let _ = c.call(Method::DELETE, format!("{u}/collections/docs"), None).await;
            c.call(
                Method::PUT,
                format!("{u}/collections/docs"),
                Some(json!({
                    "vectors": {"emb": {"size": dim, "distance": "Dot", "hnsw_config": {"m": 16, "ef_construct": 200}}},
                    "sparse_vectors": {"bm25": {"modifier": "idf"}},
                    // 8-bit codes in memory, re-scored with the full vectors (on disk), as Strata's.
                    "quantization_config": {"scalar": {"type": "int8", "always_ram": true}},
                })),
            )
            .await?;
            for (field, schema) in [
                ("key", json!("keyword")),
                ("year", json!("integer")),
                ("rating", json!("float")),
                ("category", json!("keyword")),
                (
                    "body",
                    json!({"type": "text", "tokenizer": "word", "lowercase": true, "phrase_matching": true,
                        "stemmer": {"type": "snowball", "language": "english"}, "stopwords": "english"}),
                ),
            ] {
                c.call(Method::PUT, format!("{u}/collections/docs/index?wait=true"), Some(json!({"field_name": field, "field_schema": schema}))).await?;
            }
        }
        Other::Weaviate => {
            let _ = c.call(Method::DELETE, format!("{u}/v1/schema/Doc"), None).await;
            let text = |name: &str, tok: &str, searchable: bool, filterable: bool| {
                json!({"name": name, "dataType": ["text"], "tokenization": tok, "indexSearchable": searchable, "indexFilterable": filterable})
            };
            c.call(
                Method::POST,
                format!("{u}/v1/schema"),
                Some(json!({
                    "class": "Doc", "vectorizer": "none", "vectorIndexType": "hnsw",
                    // 8-bit rotational quantization re-scoring the best 40 with the full vectors, as Strata's.
                    "vectorIndexConfig": {"distance": "dot", "efConstruction": 200, "maxConnections": 16, "ef": candidates(),
                        "rq": {"enabled": true, "bits": 8, "rescoreLimit": OVERSAMPLE * 10}},
                    "invertedIndexConfig": {"bm25": {"k1": K1, "b": B}, "stopwords": {"preset": "en"}},
                    "properties": [
                        text("key", "field", false, true),
                        text("title", "field", false, false),
                        text("body", "word", true, true),
                        {"name": "year", "dataType": ["int"], "indexRangeFilters": true},
                        text("category", "field", false, true),
                        {"name": "rating", "dataType": ["number"], "indexRangeFilters": true},
                        text("topic", "field", false, true),
                    ],
                })),
            )
            .await?;
        }
        Other::Milvus => {
            let _ = c.call(Method::POST, format!("{u}/v2/vectordb/collections/drop"), Some(json!({"collectionName": "docs"}))).await;
            let var = |name: &str, len: usize| json!({"fieldName": name, "dataType": "VarChar", "elementTypeParams": {"max_length": len}});
            c.call(
                Method::POST,
                format!("{u}/v2/vectordb/collections/create"),
                Some(json!({
                    "collectionName": "docs",
                    "schema": {"autoId": false, "enableDynamicField": false, "fields": [
                        {"fieldName": "key", "dataType": "VarChar", "isPrimary": true, "elementTypeParams": {"max_length": 128}},
                        var("title", 65535),
                        {"fieldName": "body", "dataType": "VarChar", "elementTypeParams": {"max_length": 65535,
                            "enable_analyzer": true, "enable_match": true, "analyzer_params": {"type": "english"}}},
                        {"fieldName": "year", "dataType": "Int64"},
                        var("category", 16),
                        {"fieldName": "rating", "dataType": "Float"},
                        var("topic", 16),
                        {"fieldName": "emb", "dataType": "FloatVector", "elementTypeParams": {"dim": dim}},
                        {"fieldName": "sparse", "dataType": "SparseFloatVector"},
                    ],
                    "functions": [{"name": "bm25", "type": "BM25", "inputFieldNames": ["body"], "outputFieldNames": ["sparse"], "params": {}}]},
                    "indexParams": [
                        // 8-bit codes re-scored with the full vectors (refine), as Strata's.
                        {"fieldName": "emb", "indexName": "emb", "metricType": "IP", "indexType": "HNSW_SQ",
                         "params": {"M": 16, "efConstruction": 200, "sq_type": "SQ8", "refine": true, "refine_type": "FP32"}},
                        {"fieldName": "sparse", "indexName": "sparse", "metricType": "BM25", "indexType": "SPARSE_INVERTED_INDEX",
                         "params": {"inverted_index_algo": "DAAT_MAXSCORE", "bm25_k1": K1, "bm25_b": B}},
                        {"fieldName": "year", "indexName": "year", "indexType": "STL_SORT"},
                        {"fieldName": "rating", "indexName": "rating", "indexType": "STL_SORT"},
                        {"fieldName": "category", "indexName": "category", "indexType": "INVERTED"},
                    ],
                })),
            )
            .await?;
        }
        Other::Manticore => {
            c.sql(u, "DROP TABLE IF EXISTS docs").await?;
            c.sql(
                u,
                &format!(
                    "CREATE TABLE docs (key string, title string, body text indexed, year int, category string, rating float, topic string, \
                     emb float_vector knn_type='hnsw' knn_dims='{dim}' hnsw_similarity='IP' hnsw_m='16' hnsw_ef_construction='200' quantization='8bit') \
                     morphology='stem_en' stopwords='en' min_infix_len='2'"
                ),
            )
            .await?;
        }
        Other::Paradedb | Other::Postgres => {
            let ext = if o == Other::Paradedb { "CREATE EXTENSION IF NOT EXISTS pg_search; " } else { "" };
            let tsv = if o == Other::Postgres { ", tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED" } else { "" };
            c.sql(
                u,
                &format!(
                    "DROP TABLE IF EXISTS docs; CREATE EXTENSION IF NOT EXISTS vector; {ext}\
                     CREATE TABLE docs (key text NOT NULL, title text, body text, year int, category text, rating real, topic text, emb vector({dim}){tsv})"
                ),
            )
            .await?;
        }
        Other::Redis => {
            c.redis(u, &["FLUSHALL"]).await?;
            let dim = dim.to_string();
            let mut args = vec!["FT.CREATE", "idx", "ON", "HASH", "PREFIX", "1", "d:", "LANGUAGE", "english", "STOPWORDS"];
            let n = CLASSIC_STOPWORDS.len().to_string();
            args.push(&n);
            args.extend(CLASSIC_STOPWORDS.iter().copied());
            args.extend([
                "SCHEMA", "body", "TEXT", "key", "TAG", "year", "NUMERIC", "category", "TAG", "rating", "NUMERIC", "emb", "VECTOR", "HNSW", "10", "TYPE", "FLOAT16", "DIM", &dim,
                "DISTANCE_METRIC", "IP", "M", "16", "EF_CONSTRUCTION", "200",
            ]);
            c.redis(u, &args).await?;
        }
    }
    Ok(())
}

/// Waits for a Meilisearch task (the answer to a write) to end.
async fn meili_wait(c: &mut Conns, u: &str, task: &Json) -> Result<(), String> {
    let Some(id) = task["taskUid"].as_u64() else { return Ok(()) };
    loop {
        let t = c.call(Method::GET, format!("{u}/tasks/{id}"), None).await?;
        match t["status"].as_str() {
            Some("succeeded") | Some("canceled") => return Ok(()),
            Some("failed") => return Err(format!("task {id}: {}", t["error"])),
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

/// What makes every loaded document searchable and indexed, timed step by step.
async fn finish(o: Other, c: &mut Conns, u: &str, docs: usize) -> Result<Json, String> {
    let mut steps = serde_json::Map::new();
    let mut step = |name: &str, t: Instant| {
        steps.insert(name.to_string(), json!(t.elapsed().as_secs_f64()));
        eprintln!("{}: {name} in {:.1} s", o.name(), t.elapsed().as_secs_f64());
    };
    match o {
        Other::Meili => {
            // The documents are indexed by tasks, after their writes return.
            let t = Instant::now();
            loop {
                let j = c.call(Method::GET, format!("{u}/tasks?statuses=enqueued,processing&limit=1"), None).await?;
                if j["total"].as_u64() == Some(0) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            step("index_s", t);
            // This load's document tasks come after the index's settings (`setup`'s last task).
            let since = c.call(Method::GET, format!("{u}/tasks?types=settingsUpdate&indexUids=docs&limit=1"), None).await?["results"][0]["uid"].as_u64().unwrap_or(0);
            let f = c.call(Method::GET, format!("{u}/tasks?statuses=failed&types=documentAdditionOrUpdate&indexUids=docs&limit=1"), None).await?;
            if f["results"][0]["uid"].as_u64().is_some_and(|id| id > since) {
                return Err(format!("indexing failed: {}", f["results"][0]["error"]));
            }
        }
        Other::Qdrant => {
            // Searchable once written; the optimizer builds the HNSW graphs and merges segments after.
            let t = Instant::now();
            let mut green = 0;
            while green < 3 {
                let j = c.call(Method::GET, format!("{u}/collections/docs"), None).await?;
                let r = &j["result"];
                let indexed = r["indexed_vectors_count"].as_u64().unwrap_or(0);
                if r["status"] == json!("green") && r["optimizer_status"] == json!("ok") && indexed >= docs as u64 {
                    green += 1;
                } else {
                    green = 0;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            step("index_s", t);
        }
        Other::Milvus => {
            let t = Instant::now();
            c.call(Method::POST, format!("{u}/v2/vectordb/collections/flush"), Some(json!({"collectionName": "docs"}))).await?;
            step("flush_s", t);
            let t = Instant::now();
            for index in ["emb", "sparse"] {
                loop {
                    let j = c.call(Method::POST, format!("{u}/v2/vectordb/indexes/describe"), Some(json!({"collectionName": "docs", "indexName": index}))).await?;
                    let d = &j["data"][0];
                    if d["indexState"] == json!("Finished") && d["pendingRows"].as_u64() == Some(0) && d["indexedRows"].as_u64() >= Some(docs as u64) {
                        break;
                    }
                    if d["indexState"] == json!("Failed") {
                        return Err(format!("index {index}: {d}"));
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            step("index_s", t);
            let t = Instant::now();
            c.call(Method::POST, format!("{u}/v2/vectordb/collections/load"), Some(json!({"collectionName": "docs"}))).await?;
            loop {
                let j = c.call(Method::POST, format!("{u}/v2/vectordb/collections/get_load_state"), Some(json!({"collectionName": "docs"}))).await?;
                if j["data"]["loadState"] == json!("LoadStateLoaded") {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            step("load_s", t);
        }
        Other::Manticore => {
            let t = Instant::now();
            c.sql(u, "FLUSH RAMCHUNK docs").await?;
            step("flush_s", t);
            let t = Instant::now();
            c.sql(u, "OPTIMIZE TABLE docs OPTION cutoff=1, sync=1").await?;
            step("merge_s", t);
        }
        Other::Paradedb | Other::Postgres => {
            let set = "SET maintenance_work_mem = '2560MB'; SET max_parallel_maintenance_workers = 2; ";
            let t = Instant::now();
            c.sql(u, "ALTER TABLE docs ADD PRIMARY KEY (key)").await?;
            step("primary_key_s", t);
            if o == Other::Paradedb {
                let t = Instant::now();
                c.sql(
                    u,
                    &format!(
                        "{set}CREATE INDEX docs_search ON docs USING paradedb (key, \
                         (body::pdb.simple('stemmer=english', 'stopwords_language=english', 'ascii_folding=true')), \
                         year, (category::pdb.literal), rating, emb vector_ip_ops)"
                    ),
                )
                .await?;
                step("search_index_s", t);
            } else {
                let t = Instant::now();
                c.sql(u, &format!("{set}CREATE INDEX docs_tsv ON docs USING gin (tsv)")).await?;
                step("gin_index_s", t);
                let t = Instant::now();
                c.sql(u, &format!("{set}CREATE INDEX docs_emb ON docs USING hnsw (emb vector_ip_ops) WITH (m = 16, ef_construction = 200)")).await?;
                step("hnsw_index_s", t);
            }
            let t = Instant::now();
            c.sql(u, "VACUUM ANALYZE docs").await?;
            step("analyze_s", t);
        }
        Other::Redis => {
            // Vectors are indexed in the background after their writes return.
            let t = Instant::now();
            loop {
                let info = redis_map(&c.redis(u, &["FT.INFO", "idx"]).await?);
                let pct = info.get("percent_indexed").and_then(|v| v.as_str().and_then(|x| x.parse::<f64>().ok()).or(v.as_f64())).unwrap_or(0.0);
                let busy = info.get("indexing").map(|v| v.to_string().trim_matches('"') != "0").unwrap_or(false);
                if pct >= 1.0 && !busy {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            step("index_s", t);
        }
        // Typesense and Weaviate index each batch before answering it.
        Other::Typesense | Other::Weaviate => {}
    }
    Ok(Json::Object(steps))
}

/// A flat [key, value, key, value, ...] reply as a map.
fn redis_map(j: &Json) -> BTreeMap<String, Json> {
    let mut m = BTreeMap::new();
    if let Some(a) = j.as_array() {
        for p in a.chunks(2) {
            if let (Some(k), Some(v)) = (p[0].as_str(), p.get(1)) {
                m.insert(k.to_string(), v.clone());
            }
        }
    }
    m
}

/// What the engine says of the index: documents searchable, and its size where it reports one.
async fn stats(o: Other, c: &mut Conns, u: &str) -> Result<Json, String> {
    Ok(match o {
        Other::Meili => {
            let i = c.call(Method::GET, format!("{u}/indexes/docs/stats"), None).await?;
            let all = c.call(Method::GET, format!("{u}/stats"), None).await?;
            json!({"searchable_docs": i["numberOfDocuments"], "index_bytes": all["databaseSize"], "engine_stats": i})
        }
        Other::Typesense => {
            let i = c.call(Method::GET, format!("{u}/collections/docs"), None).await?;
            let m = c.call(Method::GET, format!("{u}/metrics.json"), None).await.unwrap_or(Json::Null);
            json!({"searchable_docs": i["num_documents"], "memory": m})
        }
        Other::Qdrant => {
            let i = c.call(Method::GET, format!("{u}/collections/docs"), None).await?;
            let n = c.call(Method::POST, format!("{u}/collections/docs/points/count"), Some(json!({"exact": true}))).await?;
            json!({"searchable_docs": n["result"]["count"], "segments": i["result"]["segments_count"], "engine_stats": i["result"]})
        }
        Other::Weaviate => {
            let j = c.call(Method::POST, format!("{u}/v1/graphql"), Some(json!({"query": "{ Aggregate { Doc { meta { count } } } }"}))).await?;
            json!({"searchable_docs": j["data"]["Aggregate"]["Doc"][0]["meta"]["count"]})
        }
        Other::Milvus => {
            let j = c.call(Method::POST, format!("{u}/v2/vectordb/collections/get_stats"), Some(json!({"collectionName": "docs"}))).await?;
            let q = c.call(Method::POST, format!("{u}/v2/vectordb/entities/query"), Some(json!({"collectionName": "docs", "filter": "", "outputFields": ["count(*)"]}))).await?;
            json!({"searchable_docs": q["data"][0]["count(*)"], "engine_stats": j["data"]})
        }
        Other::Manticore => {
            let j = c.sql(u, "SHOW TABLE docs STATUS").await?;
            let mut m = serde_json::Map::new();
            for r in j[0]["data"].as_array().cloned().unwrap_or_default() {
                if let (Some(k), Some(v)) = (r["Variable_name"].as_str(), r.get("Value")) {
                    m.insert(k.to_string(), v.clone());
                }
            }
            let num = |k: &str| m.get(k).and_then(|v| v.as_str().and_then(|x| x.parse::<u64>().ok()).or(v.as_u64()));
            json!({"searchable_docs": num("indexed_documents"), "index_bytes": num("disk_bytes"), "ram_bytes": num("ram_bytes"), "engine_stats": m})
        }
        Other::Paradedb | Other::Postgres => {
            let j = c.sql(u, "SELECT count(*), pg_total_relation_size('docs'), pg_relation_size('docs'), pg_indexes_size('docs') FROM docs").await?;
            let r = &j["rows"][0];
            json!({"searchable_docs": r[0], "index_bytes": r[1], "table_bytes": r[2], "indexes_bytes": r[3]})
        }
        Other::Redis => {
            let info = redis_map(&c.redis(u, &["FT.INFO", "idx"]).await?);
            let mem = c.redis(u, &["INFO", "memory"]).await?;
            let used = mem.as_str().and_then(|t| t.lines().find_map(|l| l.strip_prefix("used_memory:")).and_then(|v| v.trim().parse::<u64>().ok()));
            let n = info.get("num_docs").and_then(|v| v.as_str().and_then(|x| x.parse::<u64>().ok()).or(v.as_u64()));
            json!({"searchable_docs": n, "memory_bytes": used, "engine_stats": info})
        }
    })
}

/// Errors worth sending a bulk request again for: the engine pushing back or not answering.
fn retryable(e: &str) -> bool {
    e.starts_with("429") || e.starts_with("503") || e.starts_with("502") || e.contains("error sending request") || e.contains("timed out") || e.contains("connection closed")
}

#[allow(clippy::too_many_arguments)]
pub async fn load(collection: &str, o: Other, urls: &[String], batch: usize, clients: usize, limit: Option<usize>, out: &Path) -> Result<(), String> {
    let dim = meta(collection)?.dim;
    let mut c = Conns::new(o, Duration::from_secs(6 * 3600))?;
    let u = urls[0].clone();
    // Qdrant's BM25 is computed here, with the collection's average document length.
    let (avgdl, prepare_s) = if o == Other::Qdrant {
        let t = Instant::now();
        let a = avg_len(collection, limit)?;
        eprintln!("qdrant: average body length {a:.1} terms ({:.1} s)", t.elapsed().as_secs_f64());
        (a, t.elapsed().as_secs_f64())
    } else {
        (1.0, 0.0)
    };
    let t_setup = Instant::now();
    setup(o, &mut c, &u, dim).await?;
    let setup_s = t_setup.elapsed().as_secs_f64();
    eprintln!("{}: index created in {setup_s:.1} s", o.name());

    let t = Instant::now();
    let (tx, rx) = tokio::sync::mpsc::channel::<Req>(clients * 2);
    let coll = collection.to_string();
    let us = urls.to_vec();
    let producer = std::thread::spawn(move || -> Result<(usize, u64), String> {
        let (mut rows, mut first, mut bytes, mut sent) = (Vec::new(), 0usize, 0u64, 0usize);
        let mut flush = |rows: &mut Vec<(RawDoc, Vec<f32>)>, first: usize| -> Result<(), String> {
            let r = batch_req(o, &us[sent % us.len()], rows, &|i, _| (first + i) as u64, avgdl);
            sent += 1;
            bytes += r.bytes() as u64;
            rows.clear();
            tx.blocking_send(r).map_err(s)
        };
        let n = each_doc(&coll, limit, |r, v| {
            rows.push((r, v));
            if rows.len() == batch {
                flush(&mut rows, first)?;
                first += batch;
            }
            Ok(())
        })?;
        if !rows.is_empty() {
            flush(&mut rows, first)?;
        }
        Ok((n, bytes))
    });
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let done = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..clients {
        let (rx, done, mut conn) = (rx.clone(), done.clone(), c.fork());
        tasks.push(tokio::spawn(async move {
            loop {
                let Some(req) = rx.lock().await.recv().await else { return Ok::<(), String>(()) };
                let mut tries = 0;
                loop {
                    tries += 1;
                    match conn.exec(&req).await {
                        Ok(_) => break,
                        Err(e) if tries < 30 && retryable(&e) => tokio::time::sleep(Duration::from_secs(1)).await,
                        Err(e) => return Err(format!("bulk: {e}")),
                    }
                }
                let docs = (done.fetch_add(1, Ordering::Relaxed) + 1) * batch;
                if docs % 25_000 < batch {
                    eprintln!("{}: {docs} documents sent, {:.0} s", o.name(), t.elapsed().as_secs_f64());
                }
            }
        }));
    }
    for task in tasks {
        task.await.map_err(s)??;
    }
    let (docs, bytes) = producer.join().map_err(|_| "reader thread panicked".to_string())??;
    let ingest_s = t.elapsed().as_secs_f64();
    eprintln!("{}: {docs} documents ({:.1} GB) sent in {ingest_s:.1} s", o.name(), bytes as f64 / 1e9);
    let t1 = Instant::now();
    let steps = finish(o, &mut c, &u, docs).await?;
    let merge_s = t1.elapsed().as_secs_f64();
    let total_s = t.elapsed().as_secs_f64();
    eprintln!("{}: indexed {merge_s:.1} s after the last write; {total_s:.1} s in all", o.name());
    let mut report = json!({
        "engine": o.name(), "collection": collection, "docs": docs, "request_bytes": bytes, "batch": batch, "clients": clients,
        "prepare_s": prepare_s, "setup_s": setup_s, "ingest_s": ingest_s, "refresh_s": 0.0, "merge_s": merge_s, "total_s": total_s, "steps": steps,
    });
    if let Json::Object(st) = stats(o, &mut c, &u).await? {
        report.as_object_mut().unwrap().extend(st);
    }
    if report["searchable_docs"].as_u64() != Some(docs as u64) {
        eprintln!("{}: WARNING: {docs} documents sent, {} searchable", o.name(), report["searchable_docs"]);
    }
    std::fs::create_dir_all(out).map_err(s)?;
    let path = out.join(format!("load-{}.json", o.name()));
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).map_err(s)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

// --- A replay's writes ----------------------------------------------------

/// The number Qdrant and Manticore know a document a replay inserts by
/// (Manticore's id is one more, as for a loaded row): from its key,
/// `sim<6 hex>[w]-<n>` (`engine-compare plan`), above every row of a
/// collection and below 2^53, so that any JSON reader keeps it exact.
/// Another key gets a hash, above those.
pub(crate) fn new_id(key: &str) -> u64 {
    let parsed = key.strip_prefix("sim").and_then(|rest| {
        let (p, n) = rest.rsplit_once('-')?;
        let warm = match p.len() {
            6 => 0,
            7 if p.ends_with('w') => 1,
            _ => return None,
        };
        let h = u64::from_str_radix(&p[..6], 16).ok()?;
        let n: u64 = n.parse().ok()?;
        (n < 1 << 28).then_some((warm << 48) | ((h & 0xF_FFFF) << 28) | n)
    });
    match parsed {
        Some(x) => (1 << 50) | x,
        None => (1 << 51) | (twox_hash::XxHash3_64::oneshot(key.as_bytes()) & ((1 << 50) - 1)),
    }
}

/// New documents (each `RawDoc`'s id its key) as the engine's bulk request;
/// `avgdl` is the collection's, for Qdrant's BM25 weights.
pub(crate) fn insert_req(o: Other, url: &str, docs: &[(RawDoc, Vec<f32>)], avgdl: f64) -> Req {
    batch_req(o, url, docs, &|_, r| new_id(&r.id), avgdl)
}

/// A document's rating set to `rating` (a replay's update, or putting it
/// back). The document is `key`, at `row` of the collection; Milvus has no
/// partial update and is sent the whole document (`doc`) again, as
/// `writes` sends Elasticsearch, OpenSearch and Solr.
pub(crate) fn rating_req(o: Other, url: &str, key: &str, row: u64, rating: f64, doc: Option<&(RawDoc, Vec<f32>)>) -> Result<Req, String> {
    let http = |method: Method, url: String, body: Json| Req::Http { method, url, ctype: "application/json", body: serde_json::to_vec(&body).unwrap() };
    Ok(match o {
        // Adds or updates: the fields given change, the others stay.
        Other::Meili => http(Method::PUT, format!("{url}/indexes/docs/documents?primaryKey=id"), json!([{"id": meili_id(key), "rating": rating}])),
        Other::Typesense => http(Method::PATCH, format!("{url}/collections/docs/documents/{}", id_path(key)), json!({"rating": rating})),
        Other::Qdrant => http(Method::POST, format!("{url}/collections/docs/points/payload?wait=true"), json!({"payload": {"rating": rating}, "points": [row]})),
        Other::Weaviate => http(Method::PATCH, format!("{url}/v1/objects/Doc/{}", uuid_of(key)), json!({"class": "Doc", "properties": {"rating": rating}})),
        Other::Milvus => {
            let (r, v) = doc.ok_or("Milvus updates a whole document, and none was given")?;
            let mut r = r.clone();
            r.id = key.to_string();
            r.rating = rating;
            match batch_req(o, url, &[(r, v.clone())], &|_, _| row, 1.0) {
                Req::Http { method, url, ctype, body } => Req::Http { method, url: url.replace("/entities/insert", "/entities/upsert"), ctype, body },
                other => other,
            }
        }
        // An attribute changed in place.
        Other::Manticore => post(format!("{url}/update"), &json!({"table": "docs", "id": row + 1, "doc": {"rating": rating}})),
        Other::Paradedb | Other::Postgres => Req::Sql { url: url.to_string(), sql: format!("UPDATE docs SET rating = {rating} WHERE key = {}", sql_str(key)) },
        Other::Redis => Req::Redis {
            url: url.to_string(),
            cmds: vec![vec![b"HSET".to_vec(), format!("d:{key}").into_bytes(), b"rating".to_vec(), rating.to_string().into_bytes()]],
        },
    })
}

/// Documents a replay inserted, deleted (keys as [`insert_req`] wrote
/// them), as the requests to send one after the other.
pub(crate) fn delete_reqs(o: Other, url: &str, keys: &[String]) -> Vec<Req> {
    if keys.is_empty() {
        return Vec::new();
    }
    let none = |method: Method, url: String| Req::Http { method, url, ctype: "application/json", body: Vec::new() };
    match o {
        Other::Meili => vec![post(format!("{url}/indexes/docs/documents/delete-batch"), &json!(keys.iter().map(|k| meili_id(k)).collect::<Vec<_>>()))],
        Other::Typesense => {
            let ids: Vec<String> = keys.iter().map(|k| format!("`{k}`")).collect();
            vec![none(Method::DELETE, format!("{url}/collections/docs/documents?filter_by={}", query_escape(&format!("id:[{}]", ids.join(",")))))]
        }
        Other::Qdrant => vec![post(format!("{url}/collections/docs/points/delete?wait=true"), &json!({"points": keys.iter().map(|k| new_id(k)).collect::<Vec<_>>()}))],
        Other::Weaviate => keys.iter().map(|k| none(Method::DELETE, format!("{url}/v1/objects/Doc/{}", uuid_of(k)))).collect(),
        Other::Milvus => vec![post(format!("{url}/v2/vectordb/entities/delete"), &json!({"collectionName": "docs", "filter": format!("key in {}", json!(keys))}))],
        Other::Manticore => {
            let ids: Vec<String> = keys.iter().map(|k| (new_id(k) + 1).to_string()).collect();
            vec![manticore_sql(url, &format!("DELETE FROM docs WHERE id IN ({})", ids.join(",")))]
        }
        Other::Paradedb | Other::Postgres => {
            let keys: Vec<String> = keys.iter().map(|k| sql_str(k)).collect();
            vec![Req::Sql { url: url.to_string(), sql: format!("DELETE FROM docs WHERE key IN ({})", keys.join(", ")) }]
        }
        Other::Redis => {
            let mut del = vec![b"DEL".to_vec()];
            del.extend(keys.iter().map(|k| format!("d:{k}").into_bytes()));
            vec![Req::Redis { url: url.to_string(), cmds: vec![del] }]
        }
    }
}

/// Whether an answer to a bm25 request holds the document `key` (a replay's
/// prober: Meilisearch and Typesense forgive typos, so another marked
/// document can match the word too).
pub(crate) fn finds(o: Other, j: &Json, key: &str) -> bool {
    answer(o, "bm25", j)["keys"].as_array().is_some_and(|ks| ks.iter().any(|k| k.as_str() == Some(key)))
}

/// How many documents the engine says it holds, asked every half second
/// for up to 30 s until it says `expect` (Meilisearch writes in tasks, and
/// Milvus counts with a bounded staleness).
pub(crate) async fn searchable_docs(o: Other, url: &str, expect: u64) -> Result<Option<u64>, String> {
    let mut c = Conns::new(o, Duration::from_secs(60))?;
    let t = Instant::now();
    loop {
        let n = stats(o, &mut c, url).await?["searchable_docs"].as_u64();
        if n == Some(expect) || t.elapsed() > Duration::from_secs(30) {
            return Ok(n);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The collection's average body length in terms, which Qdrant's BM25
/// weights of a replay's new documents need (as `load` gave the loaded ones).
pub(crate) fn avgdl(collection: &str) -> Result<f64, String> {
    avg_len(collection, None)
}

// --- Query kinds -----------------------------------------------------------

/// Why an engine cannot answer a kind at all (nothing is sent).
pub(crate) fn unsupported(o: Other, kind: &str) -> Option<&'static str> {
    use Other::*;
    Some(match (o, kind) {
        (Meili, "boosted (bm25 × decay)") => "no function scores: a numeric field can sort results (sort ranking rule), not multiply their relevance",
        (Meili, "sql aggregate") => "no grouped aggregates: facets count values, and facet stats give a numeric field's minimum and maximum only",
        (Typesense, "boosted (bm25 × decay)") => "no function scores: decay functions only sort (before or after the text match), they do not multiply it",
        (Typesense, "sql aggregate") => "no grouped aggregates: facets count values, and facet stats cover the whole result, not each group",
        (Manticore, "hybrid (weighted)") => "its hybrid search fuses by RRF only (fusion_method='rrf')",
        (Qdrant, "typo") => "no fuzzy matching: its text index matches whole words (or prefixes, with a prefix tokenizer)",
        (Qdrant, "prefix") => "prefixes need the prefix tokenizer, an index of its own that would replace the word index the facet and phrase kinds use on the field",
        (Qdrant, "sql aggregate") => "no grouped aggregates: its facet API counts values only",
        (Weaviate, "boosted (bm25 × decay)") => "no function scores",
        (Weaviate, "phrase") => "no phrase queries",
        (Weaviate, "typo") => "no fuzzy matching",
        (Milvus, "facet") => "no aggregations: a query counts the rows of a filter, not of each value",
        (Milvus, "typo") => "no fuzzy matching",
        (Milvus, "prefix") => "no word prefixes: TEXT_MATCH matches whole terms and LIKE a prefix of the whole string",
        (Milvus, "sql aggregate") => "no GROUP BY",
        (Postgres, "typo") => "no fuzzy matching in full-text search (pg_trgm compares whole strings, not the words of a tsvector)",
        _ => return None,
    })
}

/// What a kind means on an engine, where that is not what it means on Strata.
pub(crate) fn differs(o: Other, kind: &str) -> Option<&'static str> {
    use Other::*;
    let text_kind = matches!(kind, "bm25" | "bm25 + filter" | "phrase" | "typo" | "prefix");
    Some(match (o, kind) {
        (Meili, k) if text_kind || k == "facet" => match k {
            "facet" => "the word as a phrase (no typos, no prefix): Meilisearch does not stem, so its rows hold the word as typed",
            "prefix" => "the words before, then the prefix as the query's last word (which Meilisearch matches as a prefix); its ranking rules, words dropped from the end while too few match",
            "typo" => "its typo tolerance (1 typo from 5 letters, 2 from 9), ranked by its rules",
            _ => "Meilisearch's ranking rules (words, typo, proximity, attribute, exactness), not BM25; no stemming; typos and a prefix last word on, as by default",
        },
        (Meili, "hybrid (RRF)") => "hybrid search with semanticRatio 0.5, which merges the keyword and vector results by ranking score, not RRF",
        (Meili, "hybrid (weighted)") => "hybrid search with semanticRatio 0.8 (the keyword and vector ranking scores, merged; not min-max normalised)",
        (Typesense, k) if text_kind => match k {
            "typo" => "its typo correction (num_typos 2), ranked by its text match",
            "prefix" => "the words before, then the prefix as the query's last word (prefix true); ranked by its text match",
            _ => "Typesense's text match (words matched, then their proximity), not BM25; no typos or prefixes; words dropped from the end while too few match",
        },
        (Typesense, "hybrid (RRF)") => "rank fusion with alpha 0.5: (1 - alpha) / (1 + keyword rank) + alpha / (1 + vector rank), over the vector top 100",
        (Typesense, "hybrid (weighted)") => "rank fusion with alpha 0.8 (reciprocal ranks, not normalised scores), over the vector top 100",
        (Manticore, k) if text_kind || k == "boosted (bm25 × decay)" => match k {
            "typo" => "every word must match, each fuzzy (OPTION fuzzy=1, distance 2, preserve 1: fuzzy search takes no OR; it needs min_infix_len, set to 2)",
            "boosted (bm25 × decay)" => "weight() x the decay, where weight() is bm25a(1.2, 0.75), which Manticore scales into 0 to 1 (not BM25's raw sum), so the decay weighs more than on Strata",
            _ => "BM25 as the ranker expression bm25a(1.2, 0.75) with plain IDF (weight() is an integer: 10,000 x the score); Manticore's English stopwords",
        },
        (Manticore, "hybrid (RRF)") => "MATCH and knn fused by RRF (fusion_method='rrf', rank_constant 60, window 100)",
        (Qdrant, k) if text_kind || k == "boosted (bm25 × decay)" => match k {
            "boosted (bm25 × decay)" => "the BM25 top 100 re-ranked by a formula query: score x exp_decay(year, target 2024, scale 5, midpoint 0.5)",
            "phrase" => "BM25 over the whole query, filtered by the text index's phrase match",
            _ => "BM25 from the client: sparse vectors of Strata's analysis weighted tf (k1 + 1) / (tf + k1 (1 - b + b dl / avgdl)), Qdrant applying the IDF",
        },
        (Qdrant, "hybrid (RRF)") => "prefetch of the BM25 and vector top 100, fused by rrf (k 60)",
        (Qdrant, "hybrid (weighted)") => "prefetch of the BM25 and vector top 100, fused by dbsf (each normalised by its mean ± 3 standard deviations, summed unweighted: no weights)",
        (Weaviate, "bm25" | "bm25 + filter" | "prefix") => "Weaviate's BM25 (k1 1.2, b 0.75) without stemming (word tokenization); prefix: a Like filter on the body's words",
        (Weaviate, "facet") => "a grouped Aggregate over the rows whose body contains the word (no stemming); the 5 largest groups picked by the client",
        (Weaviate, "hybrid (RRF)") => "hybrid search with rankedFusion (k 60) and alpha 0.5",
        (Weaviate, "hybrid (weighted)") => "hybrid search with relativeScoreFusion (min-max normalised) and alpha 0.8",
        (Weaviate, "sql aggregate") => "a grouped Aggregate (GraphQL, no SQL); HAVING and ORDER BY in the client",
        (Milvus, "bm25" | "bm25 + filter" | "phrase") => "Milvus's BM25 function with its english analyzer (its stopwords, no query stopwords); phrase: PHRASE_MATCH as the filter",
        (Milvus, "boosted (bm25 × decay)") => "the BM25 top 100 re-ranked by the decay ranker (exp, origin 2024, scale 5, decay 0.5); the client keeps the top 10",
        (Milvus, "hybrid (RRF)") => "hybrid_search of the BM25 and vector top 100, rrf ranker (k 60)",
        (Milvus, "hybrid (weighted)") => "hybrid_search of the BM25 and vector top 100, weighted ranker 0.2 and 0.8 with norm_score (an arctan normalisation, not min-max)",
        (Paradedb, "typo") => "each word fuzzy with Strata's 'auto' edits, unstemmed (as Solr's), the others matched as words",
        (Paradedb, "hybrid (RRF)" | "hybrid (weighted)") => "SQL over the BM25 and vector top 100 (ParadeDB's own pattern)",
        (Paradedb, "prefix") => "a regex on the terms (pre.*), ranked with the words before",
        (Postgres, k) if text_kind || k == "boosted (bm25 × decay)" || k == "hybrid (RRF)" || k == "hybrid (weighted)" => {
            "ts_rank (term frequencies, no IDF), not BM25, over to_tsquery('english') of the words OR-ed; hybrids in SQL over the top 100 of each"
        }
        (Redis, "bm25" | "bm25 + filter" | "phrase" | "typo" | "prefix" | "boosted (bm25 × decay)") => {
            "BM25STD over Redis's English stemming; typo: %word% (1 edit) or %%word%% (2) by Strata's 'auto'; boosted: FT.AGGREGATE with ADDSCORES and APPLY"
        }
        (Redis, "hybrid (RRF)") => "FT.HYBRID, COMBINE RRF (window 100, constant 60)",
        (Redis, "hybrid (weighted)") => {
            "FT.HYBRID, COMBINE LINEAR: ALPHA 0.2 x the raw BM25STD score + BETA 0.8 x the vector score (Redis 8.8 does not normalise them), over each top 100"
        }
        (Redis, "key lookup") => "HMGET of the document's hash (its own key-value read)",
        _ => return None,
    })
}

/// The query's words OR-ed for Redis.
fn redis_or(t: &str) -> Option<String> {
    let w = word_list(t);
    (!w.is_empty()).then(|| format!("@body:({})", w.join("|")))
}

/// A TAG value for Redis, its punctuation escaped.
fn redis_tag(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if !ch.is_alphanumeric() {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The query's words OR-ed for to_tsquery.
fn ts_or(t: &str) -> Option<String> {
    let w: Vec<String> = word_list(t);
    (!w.is_empty()).then(|| sql_str(&w.join(" | ")))
}

/// The request for one kind on one engine (None: the query has nothing to
/// ask of this kind, as `request` decides for the others).
/// A key lookup by the engine's own read by id, where `request` asks a
/// search: Typesense's document GET, Qdrant's points by id (a point's id is
/// its row), Weaviate's object GET (its id is the key's uuid_of).
/// Meilisearch, Milvus, Redis, Postgres, ParadeDB and Manticore read by id
/// in `request`.
pub(crate) fn native_get(o: Other, p: &Probe, u: &str) -> Option<Req> {
    let get = |url: String| Req::Http { method: Method::GET, url, ctype: "application/json", body: Vec::new() };
    match o {
        Other::Typesense => Some(get(format!("{u}/collections/docs/documents/{}?include_fields=id,title,year,rating", query_escape(p.key)))),
        Other::Qdrant => Some(post(format!("{u}/collections/docs/points"), &json!({"ids": [p.row], "with_payload": ["key", "title", "year", "rating"]}))),
        Other::Weaviate => Some(get(format!("{u}/v1/objects/Doc/{}", uuid_of(p.key)))),
        _ => None,
    }
}

pub(crate) fn request(o: Other, kind: &str, p: &Probe, u: &str) -> Option<Req> {
    let t = &p.q.text;
    let v: &[f32] = &p.q.vector;
    let w = if kind == "facet" { match_word(t)? } else { String::new() };
    let phrase = if kind == "phrase" { Some(phrase_of(t)?) } else { None };
    let typo = if kind == "typo" { Some(typo_of(t)?) } else { None };
    let prefix = if kind == "prefix" { Some(prefix_of(t, p.knobs.prefix_len)?) } else { None };
    let (key, seen, year) = (p.key, p.seen, p.year);
    // The filters' values: `run`'s are year >= 2010 and c00 to c03, rating >= 4, and rating >= 3 for similar items.
    let f = p.knobs;
    let cats = f.cat_names();
    let quoted = |sep: &str| cats.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(sep);
    let sql_cats = cats.iter().map(|c| sql_str(c)).collect::<Vec<_>>().join(", ");
    let ws = words(t);
    match o {
        Other::Meili => {
            let base = |q: &str| json!({"q": q, "limit": 10, "attributesToRetrieve": ["id"], "showRankingScore": true});
            let vector = |vec: &[f32], ratio: f64, q: &str| with(base(q), json!({"vector": vec.to_vec(), "hybrid": {"embedder": "default", "semanticRatio": ratio}}));
            let body = match kind {
                "bm25" => base(t),
                "bm25 + filter" => with(base(t), json!({"filter": format!("year >= {} AND category IN [{}]", f.year_min, cats.join(", "))})),
                "vector" => vector(v, 1.0, ""),
                "vector + filter" => with(vector(v, 1.0, ""), json!({"filter": format!("rating >= {}", f.rating_min)})),
                "hybrid (RRF)" => vector(v, 0.5, t),
                "hybrid (weighted)" => vector(v, 0.8, t),
                "facet" => json!({"q": format!("\"{w}\""), "limit": 0, "facets": ["category"]}),
                "key lookup" => return Some(post(format!("{u}/indexes/docs/documents/fetch"), &json!({"ids": [meili_id(key)], "fields": ["id", "title", "year", "rating"]}))),
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    base(&format!("\"{a} {b}\" {ws}"))
                }
                "typo" => base(typo.as_ref().unwrap()),
                "prefix" => {
                    let (before, pre) = prefix.as_ref().unwrap();
                    base(&format!("{before} {pre}"))
                }
                "similar items" => {
                    let ids: Vec<String> = seen.iter().map(|k| format!("'{}'", meili_id(k))).collect();
                    with(vector(p.item, 1.0, ""), json!({"filter": format!("rating >= {} AND id NOT IN [{}]", f.similar_rating, ids.join(", "))}))
                }
                _ => return None,
            };
            Some(post(format!("{u}/indexes/docs/search"), &body))
        }
        Other::Typesense => {
            let text = |q: &str| json!({"collection": "docs", "q": q, "query_by": "body", "per_page": 10, "include_fields": "id", "prefix": false, "num_typos": 0, "stopwords": "query"});
            let vq = |vec: &[f32], k: usize, extra: &str| format!("emb:({}, k:{k}{extra})", vector_text(vec));
            let vector = |vec: &[f32]| json!({"collection": "docs", "q": "*", "per_page": 10, "include_fields": "id", "vector_query": vq(vec, 10, &format!(", ef:{}", candidates()))});
            let s = match kind {
                "bm25" => text(t),
                "bm25 + filter" => with(text(t), json!({"filter_by": format!("year:>={} && category:[{}]", f.year_min, cats.join(","))})),
                "vector" => vector(v),
                "vector + filter" => with(vector(v), json!({"filter_by": format!("rating:>={}", f.rating_min)})),
                "hybrid (RRF)" => with(text(t), json!({"vector_query": vq(v, DEPTH, ", alpha:0.5")})),
                "hybrid (weighted)" => with(text(t), json!({"vector_query": vq(v, DEPTH, ", alpha:0.8")})),
                "facet" => json!({"collection": "docs", "q": w, "query_by": "body", "per_page": 0, "prefix": false, "num_typos": 0, "drop_tokens_threshold": 0,
                    "facet_by": "category", "max_facet_values": 5}),
                "key lookup" => json!({"collection": "docs", "q": "*", "filter_by": format!("id:=`{key}`"), "per_page": 10, "include_fields": "id,title,year,rating"}),
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    text(&format!("\"{a} {b}\" {ws}"))
                }
                "typo" => with(text(typo.as_ref().unwrap()), json!({"num_typos": 2})),
                "prefix" => {
                    let (before, pre) = prefix.as_ref().unwrap();
                    with(text(&format!("{before} {pre}")), json!({"prefix": true}))
                }
                "similar items" => {
                    let ids: Vec<String> = seen.iter().map(|k| format!("`{k}`")).collect();
                    with(vector(p.item), json!({"filter_by": format!("rating:>={} && id:!=[{}]", f.similar_rating, ids.join(","))}))
                }
                _ => return None,
            };
            Some(post(format!("{u}/multi_search"), &json!({"searches": [s]})))
        }
        Other::Qdrant => {
            let q = format!("{u}/collections/docs/points/query");
            let vparams = json!({"hnsw_ef": candidates(), "quantization": {"rescore": true, "oversampling": OVERSAMPLE}});
            let sparse = || query_sparse(t);
            let text = |filter: Option<Json>| -> Option<Json> {
                let mut b = json!({"query": sparse()?, "using": "bm25", "limit": 10, "with_payload": ["key"]});
                if let Some(f) = filter {
                    b["filter"] = f;
                }
                Some(b)
            };
            let vector = |vec: &[f32], filter: Option<Json>| {
                let mut b = json!({"query": vec.to_vec(), "using": "emb", "limit": 10, "with_payload": ["key"], "params": vparams});
                if let Some(f) = filter {
                    b["filter"] = f;
                }
                b
            };
            let prefetch = || -> Option<Json> {
                Some(json!([
                    {"query": sparse()?, "using": "bm25", "limit": DEPTH},
                    {"query": v.to_vec(), "using": "emb", "limit": DEPTH, "params": vparams},
                ]))
            };
            let body = match kind {
                "bm25" => text(None)?,
                "bm25 + filter" => text(Some(json!({"must": [{"key": "year", "range": {"gte": f.year_min}}, {"key": "category", "match": {"any": cats}}]})))?,
                "vector" => vector(v, None),
                "vector + filter" => vector(v, Some(json!({"must": [{"key": "rating", "range": {"gte": json_num(f.rating_min)}}]}))),
                "hybrid (RRF)" => json!({"prefetch": prefetch()?, "query": {"rrf": {"k": 60}}, "limit": 10, "with_payload": ["key"]}),
                "hybrid (weighted)" => json!({"prefetch": prefetch()?, "query": {"fusion": "dbsf"}, "limit": 10, "with_payload": ["key"]}),
                "boosted (bm25 × decay)" => json!({
                    "prefetch": {"query": sparse()?, "using": "bm25", "limit": DEPTH},
                    "query": {"formula": {"mult": ["$score", {"exp_decay": {"x": "year", "target": 2024, "scale": 5, "midpoint": 0.5}}]}},
                    "limit": 10, "with_payload": ["key"],
                }),
                "facet" => {
                    return Some(post(
                        format!("{u}/collections/docs/facet"),
                        &json!({"key": "category", "filter": {"must": [{"key": "body", "match": {"text": w}}]}, "limit": 5, "exact": true}),
                    ))
                }
                "key lookup" => {
                    return Some(post(
                        format!("{u}/collections/docs/points/scroll"),
                        &json!({"filter": {"must": [{"key": "key", "match": {"value": key}}]}, "limit": 10, "with_payload": ["key", "title", "year", "rating"]}),
                    ))
                }
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    text(Some(json!({"must": [{"key": "body", "match": {"phrase": format!("{a} {b}")}}]})))?
                }
                "similar items" => vector(
                    p.item,
                    Some(json!({"must": [{"key": "rating", "range": {"gte": json_num(f.similar_rating)}}], "must_not": [{"key": "key", "match": {"any": seen}}]})),
                ),
                _ => return None,
            };
            Some(post(q, &body))
        }
        Other::Weaviate => {
            let lit = |x: &str| serde_json::to_string(x).unwrap();
            let get = |args: String, fields: &str| format!("{{ Get {{ Doc({args}) {{ key {fields} }} }} }}");
            let bm25 = |q: &str| format!("bm25: {{query: {}, properties: [\"body\"]}}", lit(q));
            let near = |vec: &[f32]| format!("nearVector: {{vector: {}}}", vector_text(vec));
            let score = "_additional { score }";
            let dist = "_additional { distance }";
            let query = match kind {
                "bm25" => get(format!("limit: 10, {}", bm25(t)), score),
                "bm25 + filter" => get(
                    format!(
                        "limit: 10, {}, where: {{operator: And, operands: [{{path: [\"year\"], operator: GreaterThanEqual, valueInt: {}}}, \
                         {{path: [\"category\"], operator: ContainsAny, valueText: [{}]}}]}}",
                        bm25(t),
                        f.year_min,
                        quoted(", ")
                    ),
                    score,
                ),
                "vector" => get(format!("limit: 10, {}", near(v)), dist),
                "vector + filter" => get(format!("limit: 10, {}, where: {{path: [\"rating\"], operator: GreaterThanEqual, valueNumber: {}}}", near(v), f.rating_min), dist),
                "hybrid (RRF)" | "hybrid (weighted)" => {
                    let (alpha, fusion) = if kind == "hybrid (RRF)" { (0.5, "rankedFusion") } else { (0.8, "relativeScoreFusion") };
                    get(format!("limit: 10, hybrid: {{query: {}, vector: {}, alpha: {alpha}, fusionType: {fusion}, properties: [\"body\"]}}", lit(t), vector_text(v)), score)
                }
                "facet" => format!(
                    "{{ Aggregate {{ Doc(groupBy: [\"category\"], where: {{path: [\"body\"], operator: ContainsAny, valueText: [{}]}}) {{ groupedBy {{ value }} meta {{ count }} }} }} }}",
                    lit(&w.to_lowercase())
                ),
                "key lookup" => get(format!("limit: 10, where: {{path: [\"key\"], operator: Equal, valueText: {}}}", lit(key)), "title year rating"),
                "prefix" => {
                    let (before, pre) = prefix.as_ref().unwrap();
                    get(format!("limit: 10, {}, where: {{path: [\"body\"], operator: Like, valueText: {}}}", bm25(before), lit(&format!("{pre}*"))), score)
                }
                "similar items" => {
                    let not: Vec<String> = seen.iter().map(|k| format!("{{path: [\"key\"], operator: NotEqual, valueText: {}}}", lit(k))).collect();
                    get(
                        format!(
                            "limit: 10, {}, where: {{operator: And, operands: [{{path: [\"rating\"], operator: GreaterThanEqual, valueNumber: {}}}, {}]}}",
                            near(p.item),
                            f.similar_rating,
                            not.join(", ")
                        ),
                        dist,
                    )
                }
                "sql aggregate" => format!(
                    "{{ Aggregate {{ Doc(groupBy: [\"category\"], where: {{path: [\"year\"], operator: GreaterThanEqual, valueInt: {year}}}) \
                     {{ groupedBy {{ value }} meta {{ count }} rating {{ mean }} }} }} }}"
                ),
                _ => return None,
            };
            Some(post(format!("{u}/v1/graphql"), &json!({"query": query})))
        }
        Other::Milvus => {
            let search = |data: Json, field: &str, filter: Option<String>, limit: usize| {
                let mut b = json!({"collectionName": "docs", "data": [data], "annsField": field, "limit": limit, "outputFields": ["key"]});
                if field == "emb" {
                    b["searchParams"] = json!({"params": {"ef": candidates(), "refine_k": OVERSAMPLE}});
                }
                if let Some(f) = filter {
                    b["filter"] = json!(f);
                }
                b
            };
            let quote = |x: &str| format!("\"{}\"", x.replace('\\', "\\\\").replace('"', "\\\""));
            let at = |path: &str, b: Json| Some(post(format!("{u}/v2/vectordb/entities/{path}"), &b));
            match kind {
                "bm25" => at("search", search(json!(t), "sparse", None, 10)),
                "bm25 + filter" => at("search", search(json!(t), "sparse", Some(format!("year >= {} and category in [{}]", f.year_min, quoted(", "))), 10)),
                "vector" => at("search", search(json!(v.to_vec()), "emb", None, 10)),
                "vector + filter" => at("search", search(json!(v.to_vec()), "emb", Some(format!("rating >= {}", f.rating_min)), 10)),
                "hybrid (RRF)" | "hybrid (weighted)" => {
                    let rerank = if kind == "hybrid (RRF)" {
                        json!({"strategy": "rrf", "params": {"k": 60}})
                    } else {
                        json!({"strategy": "weighted", "params": {"weights": [0.2, 0.8], "norm_score": true}})
                    };
                    let mut vs = json!({"data": [v.to_vec()], "annsField": "emb", "limit": DEPTH, "params": {"ef": DEPTH, "refine_k": OVERSAMPLE}});
                    vs["searchParams"] = json!({"params": {"ef": DEPTH, "refine_k": OVERSAMPLE}});
                    at(
                        "hybrid_search",
                        json!({"collectionName": "docs", "search": [{"data": [t], "annsField": "sparse", "limit": DEPTH}, vs], "rerank": rerank, "limit": 10, "outputFields": ["key"]}),
                    )
                }
                "boosted (bm25 × decay)" => {
                    let mut b = search(json!(t), "sparse", None, DEPTH);
                    b["functionScore"] = json!({"functions": [{"name": "decay", "type": "Rerank", "inputFieldNames": ["year"],
                        "params": {"reranker": "decay", "function": "exp", "origin": 2024, "scale": 5, "decay": 0.5, "offset": 0}}]});
                    at("search", b)
                }
                "key lookup" => at("get", json!({"collectionName": "docs", "id": [key], "outputFields": ["key", "title", "year", "rating"]})),
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    at("search", search(json!(t), "sparse", Some(format!("PHRASE_MATCH(body, {})", quote(&format!("{a} {b}")))), 10))
                }
                "similar items" => {
                    let ids: Vec<String> = seen.iter().map(|k| quote(k)).collect();
                    at("search", search(json!(p.item.to_vec()), "emb", Some(format!("rating >= {} and key not in [{}]", f.similar_rating, ids.join(", "))), 10))
                }
                _ => None,
            }
        }
        Other::Manticore => {
            let quorum = format!("\"{ws}\"/1");
            // BM25 (k1 1.2, b 0.75) with plain IDF (not divided by the number of query words); weight() is an integer.
            let options = "ranker=expr('10000*bm25a(1.2,0.75)'), idf='plain,tfidf_unnormalized'";
            let ranker = format!("OPTION {options}");
            let text = |m: &str, filter: &str| format!("SELECT key, weight() s FROM docs WHERE MATCH({}){filter} ORDER BY s DESC LIMIT 10 {ranker}", sql_str(m));
            let knn = |vec: &[f32], k: usize| format!("knn(emb, {k}, {}, {{ef={}, rescore=1, oversampling={}.0}})", manticore_vec(vec), candidates().max(k), OVERSAMPLE);
            let sql = match kind {
                "bm25" => text(&quorum, ""),
                "bm25 + filter" => text(&quorum, &format!(" AND year >= {} AND category IN ({sql_cats})", f.year_min)),
                "vector" => format!("SELECT key, knn_dist() d FROM docs WHERE {} LIMIT 10", knn(v, 10)),
                "vector + filter" => format!("SELECT key, knn_dist() d FROM docs WHERE {} AND rating >= {} LIMIT 10", knn(v, 10), f.rating_min),
                "hybrid (RRF)" => format!(
                    "SELECT key, hybrid_score() s FROM docs WHERE MATCH({}) AND {} LIMIT 10 OPTION fusion_method='rrf', rank_constant=60, window_size={DEPTH}, {options}",
                    sql_str(&quorum),
                    knn(v, DEPTH)
                ),
                "boosted (bm25 × decay)" => format!(
                    "SELECT key, weight() * exp({DECAY_RATE} * abs(year - 2024)) s FROM docs WHERE MATCH({}) ORDER BY s DESC LIMIT 10 {ranker}",
                    sql_str(&quorum)
                ),
                "facet" => format!("SELECT category, COUNT(*) n FROM docs WHERE MATCH({}) GROUP BY category ORDER BY n DESC LIMIT 5", sql_str(&w)),
                // By its document id (the row's number plus one, as loaded): its own read by id.
                "key lookup" => format!("SELECT key, title, year, rating FROM docs WHERE id = {}", p.row + 1),
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    text(&format!("\"{a} {b}\" MAYBE ({})", word_list(t).join(" | ")), "")
                }
                // Fuzzy search takes the words alone (no operators): all of them must match.
                "typo" => format!(
                    "SELECT key, weight() s FROM docs WHERE MATCH({}) ORDER BY s DESC LIMIT 10 OPTION fuzzy=1, distance=2, preserve=1, {options}",
                    sql_str(typo.as_ref().unwrap())
                ),
                "prefix" => {
                    let (before, pre) = prefix.as_ref().unwrap();
                    text(&format!("{pre}* MAYBE ({})", word_list(before).join(" | ")), "")
                }
                "similar items" => {
                    let ids: Vec<String> = seen.iter().map(|k| sql_str(k)).collect();
                    format!("SELECT key, knn_dist() d FROM docs WHERE {} AND rating >= {} AND key NOT IN ({}) LIMIT 10", knn(p.item, 10), f.similar_rating, ids.join(", "))
                }
                "sql aggregate" => {
                    format!("SELECT category, COUNT(*) n, AVG(rating) r FROM docs WHERE year >= {year} GROUP BY category HAVING n > 1000 ORDER BY n DESC LIMIT 10")
                }
                _ => return None,
            };
            Some(manticore_sql(u, &sql))
        }
        Other::Paradedb | Other::Postgres => {
            let pdb = o == Other::Paradedb;
            let qv = pg_vec(v);
            // The text match and its score, and the vector order.
            let (matches, score) = if pdb {
                (format!("body ||| {}", sql_str(&ws)), "pdb.score(key)".to_string())
            } else {
                let q = ts_or(t)?;
                (format!("tsv @@ to_tsquery('english', {q})"), format!("ts_rank(tsv, to_tsquery('english', {q}))"))
            };
            let all = if pdb { "key @@@ pdb.all() AND " } else { "" };
            let vector = |vec: &str, filter: &str| format!("SELECT key, emb <#> {vec} d FROM docs WHERE {all}{filter} ORDER BY emb <#> {vec} LIMIT 10").replace("WHERE  ORDER", "ORDER");
            let text = |filter: &str, extra_match: &str| format!("SELECT key, {score} s FROM docs WHERE {matches}{extra_match}{filter} ORDER BY s DESC LIMIT 10");
            let sql = match kind {
                "bm25" => text("", ""),
                "bm25 + filter" => text(&format!(" AND year >= {} AND category IN ({sql_cats})", f.year_min), ""),
                "vector" => {
                    if pdb {
                        format!("SELECT key, emb <#> {qv} d FROM docs WHERE key @@@ pdb.all() ORDER BY emb <#> {qv} LIMIT 10")
                    } else {
                        format!("SELECT key, emb <#> {qv} d FROM docs ORDER BY emb <#> {qv} LIMIT 10")
                    }
                }
                "vector + filter" => vector(&qv, &format!("rating >= {}", f.rating_min)),
                "hybrid (RRF)" => format!(
                    "WITH t AS (SELECT key, row_number() OVER (ORDER BY s DESC, key) r FROM (SELECT key, {score} s FROM docs WHERE {matches} ORDER BY s DESC LIMIT {DEPTH}) x), \
                     v AS (SELECT key, row_number() OVER (ORDER BY d, key) r FROM (SELECT key, emb <#> {qv} d FROM docs {} ORDER BY emb <#> {qv} LIMIT {DEPTH}) y) \
                     SELECT key, COALESCE(1.0 / (60 + t.r), 0) + COALESCE(1.0 / (60 + v.r), 0) s FROM t FULL OUTER JOIN v USING (key) ORDER BY s DESC, key LIMIT 10",
                    if pdb { "WHERE key @@@ pdb.all()" } else { "" }
                ),
                "hybrid (weighted)" => format!(
                    "WITH t AS (SELECT key, {score} s FROM docs WHERE {matches} ORDER BY s DESC LIMIT {DEPTH}), \
                     v AS (SELECT key, -(emb <#> {qv}) s FROM docs {} ORDER BY emb <#> {qv} LIMIT {DEPTH}), \
                     tn AS (SELECT key, (s - min(s) OVER ()) / NULLIF(max(s) OVER () - min(s) OVER (), 0) n FROM t), \
                     vn AS (SELECT key, (s - min(s) OVER ()) / NULLIF(max(s) OVER () - min(s) OVER (), 0) n FROM v) \
                     SELECT key, 0.2 * COALESCE(tn.n, 0) + 0.8 * COALESCE(vn.n, 0) s FROM tn FULL OUTER JOIN vn USING (key) ORDER BY s DESC, key LIMIT 10",
                    if pdb { "WHERE key @@@ pdb.all()" } else { "" }
                ),
                "boosted (bm25 × decay)" => format!("SELECT key, {score} * exp({DECAY_RATE} * abs(year - 2024)) s FROM docs WHERE {matches} ORDER BY s DESC LIMIT 10"),
                "facet" => {
                    let m = if pdb { format!("body ||| {}", sql_str(&w)) } else { format!("tsv @@ to_tsquery('english', {})", sql_str(&w)) };
                    format!("SELECT category, COUNT(*) n FROM docs WHERE {m} GROUP BY category ORDER BY n DESC LIMIT 5")
                }
                "key lookup" => format!("SELECT key, title, year, rating FROM docs WHERE key = {}", sql_str(key)),
                "phrase" => {
                    let (a, b) = phrase.as_ref().unwrap();
                    let ph = if pdb { format!(" AND body ### {}", sql_str(&format!("{a} {b}"))) } else { format!(" AND tsv @@ phraseto_tsquery('english', {})", sql_str(&format!("{a} {b}"))) };
                    text("", &ph)
                }
                "typo" => {
                    // ParadeDB only (Postgres has no fuzzy kind).
                    let parts: Vec<String> = word_list(typo.as_ref().unwrap())
                        .iter()
                        .map(|x| match auto_edits(x) {
                            0 => format!("body ||| {}", sql_str(x)),
                            n => format!("body === {}::pdb.fuzzy({n}, f, t)", sql_str(x)),
                        })
                        .collect();
                    format!("SELECT key, pdb.score(key) s FROM docs WHERE ({}) ORDER BY s DESC LIMIT 10", parts.join(" OR "))
                }
                "prefix" => {
                    let (before, pre) = prefix.as_ref().unwrap();
                    if pdb {
                        format!(
                            "SELECT key, pdb.score(key) s FROM docs WHERE body @@@ pdb.regex({}) AND body ||| {} ORDER BY s DESC LIMIT 10",
                            sql_str(&format!("{pre}.*")),
                            sql_str(before)
                        )
                    } else {
                        let q = ts_or(before)?;
                        format!(
                            "SELECT key, ts_rank(tsv, to_tsquery('english', {q})) s FROM docs WHERE tsv @@ to_tsquery('english', {}) ORDER BY s DESC LIMIT 10",
                            sql_str(&format!("{pre}:*"))
                        )
                    }
                }
                "similar items" => {
                    let ids: Vec<String> = seen.iter().map(|k| sql_str(k)).collect();
                    vector(&pg_vec(p.item), &format!("rating >= {} AND key NOT IN ({})", f.similar_rating, ids.join(", ")))
                }
                "sql aggregate" => format!(
                    "SELECT category, COUNT(*) n, AVG(rating)::float8 r FROM docs WHERE year >= {year} GROUP BY category HAVING COUNT(*) > 1000 ORDER BY n DESC LIMIT 10"
                ),
                _ => return None,
            };
            Some(Req::Sql { url: u.to_string(), sql })
        }
        Other::Redis => {
            let blob = f16_bytes(v);
            let cmd = |args: Vec<String>, blob: Option<Vec<u8>>| {
                let mut c: Vec<Vec<u8>> = args.into_iter().map(String::into_bytes).collect();
                if let Some(b) = blob {
                    c.push(b"PARAMS".to_vec());
                    c.push(b"2".to_vec());
                    c.push(b"v".to_vec());
                    c.push(b);
                }
                c.push(b"DIALECT".to_vec());
                c.push(b"2".to_vec());
                Some(Req::Redis { url: u.to_string(), cmds: vec![c] })
            };
            let a = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
            let search = |q: String| {
                let mut x = a(&["FT.SEARCH", "idx"]);
                x.push(q);
                x.extend(a(&["WITHSCORES", "SCORER", "BM25STD", "NOCONTENT", "LIMIT", "0", "10"]));
                x
            };
            let knn = |filter: &str| {
                let mut x = a(&["FT.SEARCH", "idx"]);
                x.push(format!("({filter})=>[KNN 10 @emb $v EF_RUNTIME {} AS d]", candidates()));
                x.extend(a(&["SORTBY", "d", "RETURN", "1", "d", "LIMIT", "0", "10"]));
                x
            };
            match kind {
                "bm25" => cmd(search(redis_or(t)?), None),
                "bm25 + filter" => cmd(search(format!("{} @year:[{} +inf] @category:{{{}}}", redis_or(t)?, f.year_min, cats.join("|"))), None),
                "vector" => cmd(knn("*"), Some(blob)),
                "vector + filter" => cmd(knn(&format!("@rating:[{} +inf]", f.rating_min)), Some(blob)),
                "hybrid (RRF)" | "hybrid (weighted)" => {
                    let mut x = a(&["FT.HYBRID", "idx", "SEARCH"]);
                    x.push(redis_or(t)?);
                    x.extend(a(&["SCORER", "BM25STD", "VSIM", "@emb", "$v", "KNN", "4", "K"]));
                    x.push(DEPTH.to_string());
                    x.extend(a(&["EF_RUNTIME"]));
                    x.push(DEPTH.to_string());
                    if kind == "hybrid (RRF)" {
                        x.extend(a(&["COMBINE", "RRF", "4", "WINDOW"]));
                        x.push(DEPTH.to_string());
                        x.extend(a(&["CONSTANT", "60"]));
                    } else {
                        x.extend(a(&["COMBINE", "LINEAR", "6", "ALPHA", "0.2", "BETA", "0.8", "WINDOW"]));
                        x.push(DEPTH.to_string());
                    }
                    x.extend(a(&["LIMIT", "0", "10"]));
                    // FT.HYBRID takes no DIALECT.
                    let mut c = cmd(x, Some(blob))?;
                    if let Req::Redis { cmds, .. } = &mut c {
                        let n = cmds[0].len();
                        cmds[0].truncate(n - 2);
                    }
                    Some(c)
                }
                "boosted (bm25 × decay)" => {
                    let mut x = a(&["FT.AGGREGATE", "idx"]);
                    x.push(redis_or(t)?);
                    x.extend(a(&["ADDSCORES", "SCORER", "BM25STD", "LOAD", "2", "@__key", "@year", "APPLY"]));
                    x.push(format!("@__score * exp({DECAY_RATE} * abs(@year - 2024))"));
                    x.extend(a(&["AS", "s", "SORTBY", "2", "@s", "DESC", "MAX", "10"]));
                    cmd(x, None)
                }
                "facet" => {
                    let mut x = a(&["FT.AGGREGATE", "idx"]);
                    x.push(format!("@body:{w}"));
                    x.extend(a(&["GROUPBY", "1", "@category", "REDUCE", "COUNT", "0", "AS", "n", "SORTBY", "2", "@n", "DESC", "MAX", "5"]));
                    cmd(x, None)
                }
                "key lookup" => Some(Req::Redis { url: u.to_string(), cmds: vec![a(&["HMGET", &format!("d:{key}"), "key", "title", "year", "rating"]).into_iter().map(String::into_bytes).collect()] }),
                "phrase" => {
                    let (pa, pb) = phrase.as_ref().unwrap();
                    cmd(search(format!("@body:\"{pa} {pb}\" {}", redis_or(t)?)), None)
                }
                "typo" => {
                    let parts: Vec<String> = word_list(typo.as_ref().unwrap())
                        .iter()
                        .map(|x| match auto_edits(x) {
                            0 => x.clone(),
                            1 => format!("%{x}%"),
                            _ => format!("%%{x}%%"),
                        })
                        .collect();
                    cmd(search(format!("@body:({})", parts.join("|"))), None)
                }
                "prefix" => {
                    let (before, pre) = prefix.as_ref().unwrap();
                    cmd(search(format!("@body:{pre}* ~{}", redis_or(before)?)), None)
                }
                "similar items" => {
                    let ids: Vec<String> = seen.iter().map(|k| redis_tag(k)).collect();
                    let blob = f16_bytes(p.item);
                    cmd(knn(&format!("@rating:[{} +inf] -@key:{{{}}}", f.similar_rating, ids.join("|"))), Some(blob))
                }
                "sql aggregate" => {
                    let mut x = a(&["FT.AGGREGATE", "idx"]);
                    x.push(format!("@year:[{year} +inf]"));
                    x.extend(a(&["GROUPBY", "1", "@category", "REDUCE", "COUNT", "0", "AS", "n", "REDUCE", "AVG", "1", "@rating", "AS", "r", "FILTER", "@n > 1000", "SORTBY", "2", "@n", "DESC", "MAX", "10"]));
                    cmd(x, None)
                }
                _ => None,
            }
        }
    }
}

fn num(v: &Json) -> Json {
    match v {
        Json::String(t) => t.parse::<f64>().map(Json::from).unwrap_or(v.clone()),
        _ => v.clone(),
    }
}

/// The 5 (facet) or 10 largest (value, count[, mean]) groups, by count, then value.
fn top_groups(mut rows: Vec<(Json, u64, Option<Json>)>, n: usize, min: u64) -> Vec<Json> {
    rows.retain(|r| r.1 > min);
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_string().cmp(&b.0.to_string())));
    rows.truncate(n);
    rows.into_iter().map(|(v, c, m)| if let Some(m) = m { json!([v, c, m]) } else { json!([v, c]) }).collect()
}

/// An answer reduced to keys, scores and rows, as `answer` makes them.
fn answer(o: Other, kind: &str, j: &Json) -> Json {
    let pairs = |hits: &[Json], key: &dyn Fn(&Json) -> Json, score: &dyn Fn(&Json) -> Json| {
        let keys: Vec<Json> = hits.iter().map(key).collect();
        let scores: Vec<Json> = hits.iter().map(score).collect();
        json!({"keys": keys, "scores": scores, "rows": []})
    };
    let rows = |r: Vec<Json>| json!({"keys": [], "scores": [], "rows": r});
    let arr = |v: &Json| v.as_array().cloned().unwrap_or_default();
    match o {
        Other::Meili => match kind {
            "facet" => {
                let d = j["facetDistribution"]["category"].as_object().cloned().unwrap_or_default();
                rows(top_groups(d.into_iter().map(|(k, c)| (json!(k), c.as_u64().unwrap_or(0), None)).collect(), 5, 0))
            }
            "key lookup" => pairs(&arr(&j["results"]), &|h| json!(meili_key(h["id"].as_str().unwrap_or(""))), &|_| Json::Null),
            _ => pairs(&arr(&j["hits"]), &|h| json!(meili_key(h["id"].as_str().unwrap_or(""))), &|h| h["_rankingScore"].clone()),
        },
        Other::Typesense if kind == "key lookup" && j.get("results").is_none() => json!({"keys": [j["id"].clone()], "scores": [Json::Null], "rows": []}),
        Other::Typesense => {
            let r = &j["results"][0];
            if kind == "facet" {
                let c = arr(&r["facet_counts"][0]["counts"]);
                return rows(c.iter().map(|x| json!([x["value"], x["count"]])).collect());
            }
            pairs(&arr(&r["hits"]), &|h| h["document"]["id"].clone(), &|h| {
                h.get("hybrid_search_info").map(|x| x["rank_fusion_score"].clone()).or_else(|| h.get("text_match").cloned()).or_else(|| h["vector_distance"].as_f64().map(|d| json!(-d))).unwrap_or(Json::Null)
            })
        }
        Other::Qdrant => match kind {
            "facet" => rows(arr(&j["result"]["hits"]).iter().map(|h| json!([h["value"], h["count"]])).collect()),
            // Points by id ({"result": [...]}), or the scroll it falls back on ({"result": {"points"}}).
            "key lookup" if j["result"].is_array() => pairs(&arr(&j["result"]), &|h| h["payload"]["key"].clone(), &|_| Json::Null),
            "key lookup" => pairs(&arr(&j["result"]["points"]), &|h| h["payload"]["key"].clone(), &|_| Json::Null),
            _ => pairs(&arr(&j["result"]["points"]), &|h| h["payload"]["key"].clone(), &|h| h["score"].clone()),
        },
        Other::Weaviate => match kind {
            "key lookup" if j.get("properties").is_some() => json!({"keys": [j["properties"]["key"].clone()], "scores": [Json::Null], "rows": []}),
            "facet" | "sql aggregate" => {
                let g = arr(&j["data"]["Aggregate"]["Doc"]);
                let (n, min) = if kind == "facet" { (5, 0) } else { (10, 1000) };
                rows(top_groups(
                    g.iter()
                        .map(|x| (x["groupedBy"]["value"].clone(), x["meta"]["count"].as_u64().unwrap_or(0), (kind == "sql aggregate").then(|| x["rating"]["mean"].clone())))
                        .collect(),
                    n,
                    min,
                ))
            }
            _ => pairs(&arr(&j["data"]["Get"]["Doc"]), &|h| h["key"].clone(), &|h| {
                let a = &h["_additional"];
                a.get("score").filter(|x| !x.is_null()).map(num).or_else(|| a["distance"].as_f64().map(|d| json!(-d))).unwrap_or(Json::Null)
            }),
        },
        Other::Milvus => {
            let mut hits = arr(&j["data"]);
            hits.truncate(10);
            pairs(&hits, &|h| h["key"].clone(), &|h| h.get("distance").cloned().unwrap_or(Json::Null))
        }
        Other::Manticore => {
            let data = arr(&j[0]["data"]);
            match kind {
                "facet" => rows(data.iter().map(|r| json!([r["category"], r["n"]])).collect()),
                "sql aggregate" => rows(data.iter().map(|r| json!([r["category"], r["n"], r["r"]])).collect()),
                _ => pairs(&data, &|h| h["key"].clone(), &|h| h.get("s").or_else(|| h.get("d")).cloned().unwrap_or(Json::Null)),
            }
        }
        Other::Paradedb | Other::Postgres => {
            let r = arr(&j["rows"]);
            match kind {
                "facet" | "sql aggregate" => rows(r),
                "vector" | "vector + filter" | "similar items" => pairs(&r, &|h| h[0].clone(), &|h| h[1].as_f64().map(|d| json!(-d)).unwrap_or(Json::Null)),
                _ => pairs(&r, &|h| h[0].clone(), &|h| h.get(1).cloned().unwrap_or(Json::Null)),
            }
        }
        Other::Redis => {
            let a = arr(j);
            let strip = |k: &Json| json!(k.as_str().map(|x| x.strip_prefix("d:").unwrap_or(x)).unwrap_or(""));
            match kind {
                "key lookup" => {
                    if a.first().is_some_and(|k| !k.is_null()) {
                        json!({"keys": [a[0]], "scores": [Json::Null], "rows": []})
                    } else {
                        json!({"keys": [], "scores": [], "rows": []})
                    }
                }
                "facet" | "sql aggregate" | "boosted (bm25 × decay)" => {
                    let groups: Vec<BTreeMap<String, Json>> = a.iter().skip(1).map(redis_map).collect();
                    match kind {
                        "facet" => rows(groups.iter().map(|g| json!([g.get("category"), num(&g["n"])])).collect()),
                        "sql aggregate" => rows(groups.iter().map(|g| json!([g.get("category"), num(&g["n"]), num(&g["r"])])).collect()),
                        _ => {
                            let keys: Vec<Json> = groups.iter().map(|g| strip(&g.get("__key").cloned().unwrap_or(Json::Null))).collect();
                            let scores: Vec<Json> = groups.iter().map(|g| num(&g.get("s").cloned().unwrap_or(Json::Null))).collect();
                            json!({"keys": keys, "scores": scores, "rows": []})
                        }
                    }
                }
                "hybrid (RRF)" | "hybrid (weighted)" => {
                    // [total_results, n, results, [[k, v, ...], ...], ...] (RESP2's map as pairs).
                    let m = redis_map(j);
                    let results = m.get("results").map(arr).unwrap_or_default();
                    let docs: Vec<BTreeMap<String, Json>> = results.iter().map(redis_map).collect();
                    let keys: Vec<Json> = docs.iter().map(|d| strip(d.get("__key").unwrap_or(&Json::Null))).collect();
                    let scores: Vec<Json> = docs.iter().map(|d| num(d.get("__score").unwrap_or(&Json::Null))).collect();
                    json!({"keys": keys, "scores": scores, "rows": []})
                }
                "vector" | "vector + filter" | "similar items" => {
                    // [total, key, [d, distance], ...]
                    let (mut keys, mut scores) = (Vec::new(), Vec::new());
                    for p in a.iter().skip(1).collect::<Vec<_>>().chunks(2) {
                        keys.push(strip(p[0]));
                        let d = p.get(1).map(|f| redis_map(f)).and_then(|m| m.get("d").cloned()).map(|d| num(&d));
                        scores.push(d.and_then(|d| d.as_f64()).map(|d| json!(1.0 - d)).unwrap_or(Json::Null));
                    }
                    json!({"keys": keys, "scores": scores, "rows": []})
                }
                _ => {
                    // [total, key, score, ...]
                    let (mut keys, mut scores) = (Vec::new(), Vec::new());
                    for p in a.iter().skip(1).collect::<Vec<_>>().chunks(2) {
                        keys.push(strip(p[0]));
                        scores.push(p.get(1).map(|x| num(x)).unwrap_or(Json::Null));
                    }
                    json!({"keys": keys, "scores": scores, "rows": []})
                }
            }
        }
    }
}

/// Sends `reqs` round-robin from `concurrency` clients for `seconds`, each
/// client with its own connections.
pub(crate) async fn drive(conns: &Conns, reqs: Arc<Vec<Req>>, concurrency: usize, seconds: f64) -> Timed {
    let next = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let t0 = Instant::now();
    let tasks: Vec<_> = (0..concurrency)
        .map(|_| {
            let (mut c, reqs, next) = (conns.fork(), reqs.clone(), next.clone());
            tokio::spawn(async move {
                let mut t = Timed::default();
                while Instant::now() < deadline {
                    let r = &reqs[next.fetch_add(1, Ordering::Relaxed) % reqs.len()];
                    let start = Instant::now();
                    let res = c.exec(r).await;
                    let ms = start.elapsed().as_secs_f64() * 1e3;
                    match res {
                        Ok(_) => t.ms.push(ms),
                        Err(e) => {
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
/// The engine's result cache counters, read before and after the timed
/// runs: Manticore's `qcache_*` status (its size limit, cached queries and
/// hits). Null for the engines that keep no result cache, or none unless a
/// request asks (Typesense's `use_cache`, which the requests here leave off).
async fn result_cache(o: Other, conns: &mut Conns, urls: &[String]) -> Json {
    match o {
        Other::Manticore => {
            let mut nodes = serde_json::Map::new();
            for u in urls {
                let v = match conns.sql(u, "SHOW STATUS LIKE 'qcache%'").await {
                    Ok(j) => j[0]["data"].clone(),
                    Err(e) => json!({"error": e}),
                };
                nodes.insert(u.clone(), v);
            }
            Json::Object(nodes)
        }
        _ => Json::Null,
    }
}

pub async fn run(collection: &str, o: Other, urls: &[String], n_queries: usize, concurrency: &[usize], seconds: f64, label: Option<String>, only: &[String], out: &Path) -> Result<(), String> {
    // As the others' run: the first queries with text, keys and items of the first 20,000 documents.
    let coll = data::load(collection, Some(20_000)).map_err(s)?;
    let queries: Vec<data::Query> = coll.queries.iter().filter(|q| !q.text.trim().is_empty()).take(n_queries).cloned().collect();
    let keys: Vec<String> = coll.docs.iter().map(|d| d[col::ID].as_str().unwrap_or("").to_string()).collect();
    let items: Vec<Arc<[f32]>> = coll.docs.iter().map(|d| Arc::from(d[col::EMB].as_vector().unwrap_or(&[]))).collect();
    let mut conns = Conns::new(o, Duration::from_secs(30))?;
    let label = label.unwrap_or_else(|| o.name().to_string());
    // Manticore's query cache keeps the result sets of queries slower than
    // qcache_thresh_msec (3 s by default) and answers their repeats from
    // memory: off while timed, as every engine's result cache.
    if o == Other::Manticore {
        for u in urls {
            conns.sql(u, "SET GLOBAL qcache_max_bytes=0").await.map_err(|e| format!("query cache off: {e}"))?;
        }
    }
    let cache_before = result_cache(o, &mut conns, urls).await;
    let (mut http, mut answers, mut query_ids, mut unavailable, mut differ, mut probes) =
        (Vec::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let mut warmups = BTreeMap::new();
    for &(kind, _) in KINDS {
        if !only.is_empty() && !only.iter().any(|k| k == kind) {
            continue;
        }
        if let Some(why) = unsupported(o, kind) {
            eprintln!("{label:<9} {kind:<24} unavailable: {why}");
            unavailable.insert(kind, why.to_string());
            continue;
        }
        let (mut ids, mut reqs, mut params, mut fallback) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (i, q) in queries.iter().enumerate() {
            let k = (i * 7919) % keys.len();
            let it = (i * 104_729 + 13) % keys.len();
            let seen: Vec<String> = std::iter::once(it).chain((1..SEEN).map(|j| (it + j * 997) % keys.len())).map(|j| keys[j].clone()).collect();
            let probe = Probe { q, key: &keys[k], row: k, item: &items[it], seen: &seen, year: 1990 + (i % 30) as i64, paid: false, knobs: Knobs::default() };
            let u = &urls[reqs.len() % urls.len()];
            if let Some(r) = request(o, kind, &probe, u) {
                // A key lookup by the engine's own read by id where its
                // request above is a search; the search is kept to fall
                // back on if the read is refused.
                let r = match (kind == "key lookup").then(|| native_get(o, &probe, u)).flatten() {
                    Some(n) => {
                        fallback.push(r);
                        n
                    }
                    None => r,
                };
                ids.push(q.id.clone());
                match kind {
                    "similar items" => params.push(json!({"item": keys[it], "seen": seen})),
                    "sql aggregate" => params.push(json!({"year": probe.year})),
                    _ => {}
                }
                reqs.push(r);
            }
        }
        if reqs.is_empty() {
            eprintln!("{label:<9} {kind:<24} no query of the collection makes a request of this kind");
            unavailable.insert(kind, "no query makes a request of this kind".to_string());
            continue;
        }
        if let Some(d) = differs(o, kind) {
            differ.insert(kind, d.to_string());
        }
        let mut got = Vec::new();
        let mut failed = None;
        for attempt in 0..2 {
            (got, failed) = (Vec::new(), None);
            for r in &reqs {
                match conns.exec(r).await {
                    Ok(j) => got.push(answer(o, kind, &j)),
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            match (&failed, attempt) {
                (Some(e), 0) if !fallback.is_empty() => {
                    eprintln!("{label:<9} {kind:<24} the read by id failed ({}): the search on the key instead", e.chars().take(300).collect::<String>());
                    differ.insert(kind, format!("a search on the key field: its read by id failed ({})", e.chars().take(200).collect::<String>()));
                    reqs = std::mem::take(&mut fallback);
                }
                _ => break,
            }
        }
        if let Some(e) = failed {
            eprintln!("{label:<9} {kind:<24} unavailable: {}", e.chars().take(300).collect::<String>());
            unavailable.insert(kind, e);
            continue;
        }
        answers.insert(kind, got);
        query_ids.insert(kind, ids);
        if !params.is_empty() {
            probes.insert(kind, params);
        }
        let reqs = Arc::new(reqs);
        let warm = warm_up(seconds, |c, secs| drive(&conns, reqs.clone(), c, secs)).await;
        eprintln!("{label:<9} {kind:<24} warm-up: p50 {} ms", warm.iter().map(|x| format!("{x:.3}")).collect::<Vec<_>>().join(", "));
        warmups.insert(kind, warm);
        for &cc in concurrency {
            let t = drive(&conns, reqs.clone(), cc, seconds).await;
            eprintln!(
                "{label:<9} {kind:<24} c={cc:<3} {:>8.0} q/s  p50 {:>7.3} ms  p90 {:>7.3} ms  p99 {:>7.3} ms  errors {}",
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
                "kind": kind, "concurrency": cc, "requests": t.ms.len(), "qps": t.ms.len() as f64 / t.seconds.max(1e-9),
                "p50_ms": percentile(&t.ms, 50.0), "p90_ms": percentile(&t.ms, 90.0), "p95_ms": percentile(&t.ms, 95.0),
                "p99_ms": percentile(&t.ms, 99.0), "p999_ms": percentile(&t.ms, 99.9), "max_ms": t.ms.iter().cloned().fold(0.0, f64::max),
                "mean_ms": mean(&t.ms), "errors": t.errors, "first_error": t.first_error,
            }));
        }
    }
    let cache = json!({"before": cache_before, "after": result_cache(o, &mut conns, urls).await});
    eprintln!("{label:<9} result cache: {cache}");
    let report = json!({
        "engine": o.name(), "label": label, "collection": collection, "urls": urls, "queries": queries.len(),
        "params": {"seconds": seconds, "concurrency": concurrency, "candidates": candidates(), "oversample": OVERSAMPLE, "hybrid_depth": DEPTH},
        "http": http, "unavailable": unavailable, "differs": differ, "query_ids": query_ids, "probes": probes, "answers": answers, "warm_up_p50_ms": warmups,
        "result_cache": cache,
    });
    std::fs::create_dir_all(out).map_err(s)?;
    let path = out.join(format!("run-{}.json", label.replace(' ', "-")));
    std::fs::write(&path, serde_json::to_string(&report).unwrap()).map_err(s)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

/// `v` as IEEE half floats, little-endian, rounded to nearest even: Redis's
/// FLOAT16 vectors. Everything Redis holds is in memory, and with FLOAT32 a
/// million 384-dimension vectors (stored in the hash and again in the HNSW
/// index) and the text do not fit in the 6 GiB the other engines get.
fn f16_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|&x| f16_bits(x).to_le_bytes()).collect()
}

fn f16_bits(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    // Keep the top bits of the mantissa (with its leading 1 for a subnormal half), rounding to nearest even.
    let (kept, shift) = if e <= 0 {
        if e < -10 {
            return sign;
        }
        (man | 0x80_0000, (14 - e) as u32)
    } else {
        (man, 13)
    };
    let mut h = (kept >> shift) | if e > 0 { (e as u32) << 10 } else { 0 };
    let rem = kept & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    if rem > half || (rem == half && h & 1 == 1) {
        h += 1; // a carry out of the mantissa raises the exponent, as it should
    }
    sign | h as u16
}

#[cfg(test)]
mod tests {
    #[test]
    fn half_floats_match_ieee() {
        // From numpy's float32 -> float16.
        let cases: [(f32, u16); 17] = [
            (0.0, 0x0), (-0.0, 0x8000), (1.0, 0x3c00), (-1.0, 0xbc00), (0.1, 0x2e66), (0.333333, 0x3555), (-0.05, 0xaa66),
            (65504.0, 0x7bff), (70000.0, 0x7c00), (1e-5, 0xa8), (6.1e-5, 0x3ff), (3e-8, 0x1), (1e-9, 0x0),
            (0.00048828125, 0x1000), (1.0009765625, 0x3c01), (1.00048828125, 0x3c00), (1.00146484375, 0x3c02),
        ];
        for (x, want) in cases {
            assert_eq!(super::f16_bits(x), want, "{x}");
        }
    }

    #[test]
    fn meili_ids_take_only_what_meili_accepts_and_give_the_key_back() {
        for key in ["msmarco_v2.1_doc_00_876874568#17_1626629849", "a__b", "_", "x_", "plain-01", "é/ü 'q'", ""] {
            let id = super::meili_id(key);
            assert!(id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "{id}");
            assert_eq!(super::meili_key(&id), key);
        }
        assert_eq!(super::meili_id("doc_00.1#2"), "doc__00_2e1_232");
    }

    #[test]
    fn new_ids_are_apart_from_rows_and_exact_in_json() {
        use super::new_id;
        let a: Vec<u64> = (0..1000).map(|n| new_id(&format!("sim00ab12-{n}"))).collect();
        let mut b = a.clone();
        b.sort_unstable();
        b.dedup();
        assert_eq!(b.len(), a.len());
        // Other schedules, the warm-up, and keys of another shape each get ids of their own.
        assert_ne!(new_id("sim00ab12-5"), new_id("sim00ab13-5"));
        assert_ne!(new_id("sim00ab12-5"), new_id("sim00ab12w-5"));
        for k in ["sim00ab12-5", "simffffffw-268435455", "sim00ab12-268435456", "msmarco_v2.1_doc_00_1#2", "simxyz-1"] {
            let id = new_id(k);
            assert!(id >= 1 << 50 && id < 1 << 53, "{k}: {id}");
        }
        assert_eq!(new_id("sim00ab12-7"), new_id("sim00ab12-7"));
    }

    #[test]
    fn replayed_writes_name_each_engine_s_document() {
        use super::*;
        let doc = RawDoc { id: "k#1".into(), title: "t".into(), text: "x".into(), year: 2001, category: "c01".into(), rating: 2.5, topic: "t01".into() };
        let pair = (doc, vec![0.5f32, 0.5]);
        let url = "http://e";
        let what = |r: &Req| match r {
            Req::Http { method, url, body, .. } => format!("{method} {url} {}", String::from_utf8_lossy(body)),
            Req::Sql { sql, .. } => sql.clone(),
            Req::Redis { cmds, .. } => cmds.iter().map(|c| c.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("; "),
        };
        let keys = vec!["sim000001-3".to_string(), "sim000001-4".to_string()];
        let id3 = new_id(&keys[0]);
        for o in [Other::Meili, Other::Typesense, Other::Manticore, Other::Qdrant, Other::Weaviate, Other::Milvus, Other::Paradedb, Other::Postgres, Other::Redis] {
            let up = what(&rating_req(o, url, "k#1", 7, 4.25, Some(&pair)).unwrap());
            let del: Vec<String> = delete_reqs(o, url, &keys).iter().map(|r| what(r)).collect();
            let all = del.join(" | ");
            let want_up = match o {
                Other::Meili => "PUT http://e/indexes/docs/documents?primaryKey=id [{\"id\":\"k_231\",\"rating\":4.25}]".to_string(),
                Other::Typesense => "PATCH http://e/collections/docs/documents/k%231 {\"rating\":4.25}".to_string(),
                Other::Qdrant => "POST http://e/collections/docs/points/payload?wait=true {\"payload\":{\"rating\":4.25},\"points\":[7]}".to_string(),
                Other::Weaviate => format!("PATCH http://e/v1/objects/Doc/{} {{\"class\":\"Doc\",\"properties\":{{\"rating\":4.25}}}}", uuid_of("k#1")),
                Other::Manticore => "POST http://e/update {\"doc\":{\"rating\":4.25},\"id\":8,\"table\":\"docs\"}".to_string(),
                Other::Paradedb | Other::Postgres => "UPDATE docs SET rating = 4.25 WHERE key = 'k#1'".to_string(),
                Other::Redis => "HSET d:k#1 rating 4.25".to_string(),
                Other::Milvus => String::new(),
            };
            if o == Other::Milvus {
                assert!(up.starts_with("POST http://e/v2/vectordb/entities/upsert ") && up.contains("\"key\":\"k#1\"") && up.contains("\"rating\":4.25") && up.contains("\"emb\":[0.5,0.5]"), "{up}");
            } else {
                assert_eq!(up, want_up, "{o:?}");
            }
            let names_both = match o {
                Other::Qdrant => all.contains(&format!("[{id3},{}]", id3 + 1)),
                Other::Manticore => all.contains(&format!("IN%20%28{}%2C{}%29", id3 + 1, id3 + 2)),
                Other::Weaviate => del.len() == 2 && all.contains(&uuid_of(&keys[1])),
                Other::Typesense => all.contains("filter_by=id%3A%5B%60sim000001-3%60%2C%60sim000001-4%60%5D"),
                Other::Redis => all == "DEL d:sim000001-3 d:sim000001-4",
                _ => all.contains("sim000001-3") && all.contains("sim000001-4"),
            };
            assert!(names_both, "{o:?}: {all}");
            assert!(delete_reqs(o, url, &[]).is_empty());
        }
        // A replay's insert numbers its documents by key, not by row.
        let mut d = pair.clone();
        d.0.id = keys[0].clone();
        assert!(what(&insert_req(Other::Qdrant, url, &[d.clone()], 10.0)).contains(&format!("\"id\":{id3}")));
        assert!(what(&insert_req(Other::Manticore, url, &[d], 10.0)).contains(&format!("\"id\":{}", id3 + 1)));
    }

    #[test]
    fn vespa_text_keeps_what_vespa_takes() {
        assert_eq!(crate::vespa_text("a\u{b}b\0c\td\ne\rf\u{fffe}g\u{1fffF}h é"), "a b c\td\ne\rf g h é");
    }
}
