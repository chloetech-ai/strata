//! The methods Java calls, by name, with their arguments as one map
//! ([`codec`]) and their answer as one value: what `Database` in the Java
//! package does natively. Nothing here touches the JVM, so the tests below
//! run them as Java would, without one.
//!
//! The conversions follow the Python package's (`strata-py/src/lib.rs`):
//! rows, schemas and options are read as JSON by the code that reads files
//! and the HTTP API ([`strata::ingest`]), except a vector given as a
//! `float[]`, which stays float32.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde_json::{Map, Value as Json};
use strata::db::{Db, SqlResult, WriteOp};
use strata::ingest::{self, Format};
use strata::query::exec::ResultSet;
use strata::query::ir::Plan;
use strata::query::sql::{ident, quoted};
use strata::query::Params;
use strata::rank_log::{Export, Log};
use strata::schema::Schema;
use strata::segment::build::BuildOptions;
use strata::sid::TrainOptions;
use strata::sqlite;
use strata::storage::{DbOptions, Layout, MergerOptions};
use strata::{FieldType, Value};

use crate::codec::{Writer, V};

/// The version of the calls and of [`codec`]: the Java package refuses a
/// native library of another (a jar and a library built apart).
pub const ABI: i64 = 1;

/// Why a call failed, which says what Java throws.
#[derive(Debug)]
pub enum Fail {
    /// `IllegalArgumentException`: the arguments are not what the method
    /// takes.
    Arg(String),
    /// `IllegalStateException`: the database was closed.
    Closed,
    /// `StrataException`: the database refused (a bad schema, query, value
    /// or file).
    Db(strata::Error),
}

impl From<strata::Error> for Fail {
    fn from(e: strata::Error) -> Self {
        Fail::Db(e)
    }
}

type R<T> = Result<T, Fail>;

fn arg<T>(m: impl Into<String>) -> R<T> {
    Err(Fail::Arg(m.into()))
}

/// Rows a write transaction carries (as `strata-server import` does).
const BATCH: usize = ingest::IMPORT_BATCH;
/// Inserts of at least this many rows, the memtable's size, are bulk-loaded
/// into new segments in one commit; fewer go through the write-ahead log
/// into the memtable, which gathers the rows of many inserts into one
/// segment when it fills (as the Python package does).
const BULK_INSERT: usize = 10_000;

/// A database opened by Java: what a `Database`'s handle points to. It
/// lives until Java frees it ([`crate::free`]), after its `close`, so a
/// call racing a close finds it closed rather than freed.
pub struct Handle {
    db: RwLock<Option<Arc<Db>>>,
    /// Embedded fields whose model did not open, with the reason.
    embed_errors: Vec<(String, String)>,
}

impl Handle {
    fn new(db: Db, models: &Path) -> Handle {
        let embed_errors = db.open_embedders(models).into_iter().map(|(f, e)| (f, e.to_string())).collect();
        // Segments merge in the background as writes add them, one merge
        // at a time, as a server's do (as strata-py's).
        db.spawn_merger(MergerOptions { threads: 1, ..MergerOptions::default() });
        Handle { db: RwLock::new(Some(Arc::new(db))), embed_errors }
    }

    fn get(&self) -> R<Arc<Db>> {
        self.db.read().unwrap_or_else(|e| e.into_inner()).clone().ok_or(Fail::Closed)
    }

    /// The handle Java keeps, with what opening it found.
    fn opened(self, report: Option<Json>) -> Vec<u8> {
        let mut w = Writer::default();
        w.map(2 + report.is_some() as usize);
        w.key("embedErrors");
        w.map(self.embed_errors.len());
        for (f, e) in &self.embed_errors {
            w.key(f);
            w.str(e);
        }
        if let Some(r) = &report {
            w.key("report");
            w.json(r);
        }
        w.key("handle");
        w.int(Box::into_raw(Box::new(self)) as i64);
        w.b
    }
}

/// The arguments each call takes, by name. A call given another is
/// refused before it runs, so that a misspelled option fails it rather than
/// being ignored by work already done.
fn takes(method: &str) -> Option<&'static [&'static str]> {
    Some(match method {
        "version" | "close" | "closed" | "embedErrors" | "tables" | "count" | "expire" | "optimize" => &[],
        "echo" => &["value"],
        "create" => &["path", "schema", "directory", "models", "threads"],
        "open" => &["path", "models", "threads"],
        "fromSqlite" => &["sqlitePath", "path", "tables", "schemas", "blobsBase64", "directory", "models", "threads"],
        "planSqlite" => &["sqlitePath", "tables", "schemas", "blobsBase64"],
        "fromParquet" => &["parquetPath", "path", "table", "schema", "key", "blobsBase64", "directory", "models", "threads"],
        "planParquet" => &["parquetPath", "table", "schema", "key", "blobsBase64"],
        "upgrade" => &["path"],
        "schema" => &["table"],
        "alterTable" => &["changes", "table"],
        "query" | "sql" | "plan" => &["sql", "params"],
        "queryLogged" => &["sql", "params", "log"],
        "exportRankLog" => &["options"],
        "run" => &["plan"],
        "insert" | "upsert" => &["rows", "table"],
        "delete" | "getMany" => &["keys", "table"],
        "write" => &["ops"],
        "importFile" => &["path", "format", "table"],
        "importSqlite" => &["path", "tables", "schemas", "blobsBase64"],
        "exportSqlite" => &["path", "tables", "fts", "indexes", "schemas"],
        "importParquet" => &["path", "table", "schema", "key", "blobsBase64"],
        "exportParquet" => &["path", "tables", "schemas"],
        "embed" => &["text", "field", "table", "image", "audio"],
        "embedTokens" => &["text", "field", "table"],
        "sidNext" => &["field", "prefixes", "where", "params", "table"],
        "trainCodebook" => &["field", "name", "table", "models", "levels", "k", "iterations", "sample", "seed"],
        _ => return None,
    })
}

/// A call's named arguments, those [`takes`] lists for it.
struct Args {
    method: String,
    given: Vec<(String, V)>,
    takes: &'static [&'static str],
}

impl Args {
    fn of(method: &str, v: V) -> R<Args> {
        let takes = takes(method).ok_or_else(|| Fail::Arg(format!("no such call: {method}")))?;
        let given = match v {
            V::Map(given) => given,
            V::Null => Vec::new(),
            v => return arg(format!("{method}: its arguments are a map, not {}", v.kind())),
        };
        if let Some((k, _)) = given.iter().find(|(k, _)| !takes.contains(&k.as_str())) {
            let list = if takes.is_empty() { "none".to_string() } else { takes.join(", ") };
            return arg(format!("{method} takes no option {k:?}; it takes {list}"));
        }
        Ok(Args { method: method.to_string(), given, takes })
    }

    /// The argument `k`, or `None` when it is missing or null.
    fn take(&mut self, k: &'static str) -> Option<V> {
        // A call reading what takes() does not list for it is a bug here.
        assert!(self.takes.contains(&k), "{} reads {k}, which takes() does not list", self.method);
        let i = self.given.iter().position(|(n, _)| n == k)?;
        Some(self.given.swap_remove(i).1).filter(|v| !v.is_null())
    }

    fn bad<T>(&self, k: &str, what: &str, v: &V) -> R<T> {
        arg(format!("{}: {k} is {what}, not {}", self.method, v.kind()))
    }

    fn need(&mut self, k: &'static str) -> R<V> {
        self.take(k).ok_or_else(|| Fail::Arg(format!("{}: {k} is missing", self.method)))
    }

    fn str(&mut self, k: &'static str) -> R<String> {
        match self.need(k)? {
            V::Str(s) => Ok(s),
            v => self.bad(k, "a string", &v),
        }
    }

    fn opt_str(&mut self, k: &'static str) -> R<Option<String>> {
        match self.take(k) {
            None => Ok(None),
            Some(V::Str(s)) => Ok(Some(s)),
            Some(v) => self.bad(k, "a string", &v),
        }
    }

    fn path(&mut self, k: &'static str) -> R<PathBuf> {
        self.str(k).map(PathBuf::from)
    }

    fn opt_path(&mut self, k: &'static str) -> R<Option<PathBuf>> {
        Ok(self.opt_str(k)?.map(PathBuf::from))
    }

    fn bool(&mut self, k: &'static str, default: bool) -> R<bool> {
        match self.take(k) {
            None => Ok(default),
            Some(V::Bool(b)) => Ok(b),
            Some(v) => self.bad(k, "a boolean", &v),
        }
    }

    fn opt_count(&mut self, k: &'static str) -> R<Option<usize>> {
        match self.take(k) {
            None => Ok(None),
            Some(V::Int(n)) if n >= 0 => Ok(Some(n as usize)),
            Some(v) => self.bad(k, "a count (an integer, 0 or more)", &v),
        }
    }

    fn count(&mut self, k: &'static str, default: usize) -> R<usize> {
        Ok(self.opt_count(k)?.unwrap_or(default))
    }

    /// One name or a list of them.
    fn names(&mut self, k: &'static str) -> R<Option<Vec<String>>> {
        match self.take(k) {
            None => Ok(None),
            Some(V::Str(s)) => Ok(Some(vec![s])),
            Some(V::List(xs)) => xs.into_iter().map(|x| if let V::Str(s) = x { Ok(s) } else { self.bad(k, "a name or a list of names", &x) }).collect::<R<_>>().map(Some),
            Some(v) => self.bad(k, "a name or a list of names", &v),
        }
    }

    /// JSON text: a string (taken as JSON text, or the path of a JSON file
    /// when it does not start like JSON), or a map or list.
    fn json_text(&mut self, k: &'static str) -> R<Option<String>> {
        match self.take(k) {
            None => Ok(None),
            Some(V::Str(s)) if s.trim_start().starts_with(['{', '[']) => Ok(Some(s)),
            Some(V::Str(path)) => std::fs::read_to_string(&path).map(Some).map_err(|e| Fail::Arg(format!("{}: {k}: {path}: {e}", self.method))),
            Some(v @ (V::Map(_) | V::List(_))) => Ok(Some(v.to_json().map_err(Fail::Arg)?.to_string())),
            Some(v) => self.bad(k, "JSON text, a file of it, a map or a list", &v),
        }
    }

    fn json(&mut self, k: &'static str) -> R<Option<Json>> {
        match self.take(k) {
            None => Ok(None),
            Some(v) => v.to_json().map(Some).map_err(|e| Fail::Arg(format!("{}: {k}: {e}", self.method))),
        }
    }

    fn list(&mut self, k: &'static str) -> R<Vec<V>> {
        match self.take(k) {
            None => Ok(Vec::new()),
            Some(V::List(xs)) => Ok(xs),
            Some(v @ V::Map(_)) => Ok(vec![v]),
            Some(v) => self.bad(k, "a list", &v),
        }
    }

    fn params(&mut self, db: &Db) -> R<Params> {
        match self.take("params") {
            None => Ok(Params::new()),
            Some(V::Map(kv)) => {
                let mut p = Params::new();
                for (k, v) in kv {
                    p = p.set(&k, param_value(db, v)?);
                }
                Ok(p)
            }
            Some(v) => self.bad("params", "a map of names to values", &v),
        }
    }

    fn bytes(&mut self, k: &'static str) -> R<Option<Vec<u8>>> {
        match self.take(k) {
            None => Ok(None),
            Some(V::Bytes(b)) => Ok(Some(b)),
            Some(v) => self.bad(k, "a byte[]", &v),
        }
    }
}

/// A query parameter: a vector or a time as it is; anything else as the
/// HTTP API reads its JSON ([`param`]).
fn param_value(db: &Db, v: V) -> R<Value> {
    Ok(match v {
        V::Vector(xs) => Value::Vector(xs.into()),
        V::Time(t) => Value::Timestamp(t),
        v => param(db, &v.to_json().map_err(Fail::Arg)?)?,
    })
}

/// A query parameter, as the HTTP API reads one: a number, string, null, a
/// list of numbers (a vector), a list, a map of term weights (SPARSE),
/// `{"embed": text, "field": f}` (the text's vector by `f`'s model, or an
/// image or sound file's for `{"embed": {"image": "<base64>"}}`) or
/// `{"vector_of": key, "field": f}` (a stored row's vector), each with
/// `"table"` for a field of another table than the first.
fn param(db: &Db, j: &Json) -> strata::Result<Value> {
    let bad = |m: String| strata::Error::Query(m);
    Ok(match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(x) => Value::Int(x),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => Value::Str(s.clone()),
        // Whole numbers stay exact integers (`IN (:ids)` with 64-bit ids);
        // where a vector is expected, a list of numbers is one.
        Json::Array(xs) if !xs.is_empty() && xs.iter().all(|x| x.is_i64()) => Value::List(xs.iter().map(|x| Value::Int(x.as_i64().unwrap_or(0))).collect()),
        Json::Array(xs) if !xs.is_empty() && xs.iter().all(|x| x.is_number()) => Value::Vector(xs.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect::<Vec<_>>().into()),
        Json::Array(xs) => Value::List(xs.iter().map(|x| param(db, x)).collect::<strata::Result<_>>()?),
        Json::Object(o) if o.get("embed").is_some_and(|e| e.is_string() || e.get("image").is_some() || e.get("audio").is_some()) && o.keys().all(|k| k == "embed" || k == "field" || k == "table") => {
            let s = table_of(db, o.get("table").and_then(|t| t.as_str()))?;
            let field = match o.get("field").and_then(|f| f.as_str()) {
                Some(f) => f.to_string(),
                None => s.fields.iter().find(|f| db.embedder_in(&s.table, &f.name).is_some()).map(|f| f.name.clone()).ok_or_else(|| bad("no embedded field has its model open".into()))?,
            };
            let e = db.embedder_in(&s.table, &field).ok_or_else(|| bad(format!("{field} has no embedding model open (see Database.Options.models)")))?;
            match &o["embed"] {
                // A multi-vector field's: the list of the tokens' vectors.
                Json::String(t) => db.embed_query_in(&s.table, &field, t)?,
                Json::Object(i) if i.len() == 1 && i.values().all(|v| v.is_string()) => {
                    let (kind, b64) = i.iter().next().expect("one key");
                    let (_, data) = strata::embed::query_file(kind, b64.as_str().unwrap_or_default())?;
                    Value::Vector(strata::embed::embed_file(e.as_ref(), &data)?.into())
                }
                _ => return Err(bad("\"embed\" is a text, {\"image\": \"<base64>\"} or {\"audio\": \"<base64>\"}".into())),
            }
        }
        Json::Object(o) if o.contains_key("vector_of") && o.keys().all(|k| k == "vector_of" || k == "field" || k == "table") => {
            let s = &*table_of(db, o.get("table").and_then(|t| t.as_str()))?;
            let pk = s.primary_key.ok_or_else(|| bad("vector_of needs a primary key".into()))?;
            let field = match o.get("field").and_then(|f| f.as_str()) {
                Some(f) => f.to_string(),
                None => s.fields.iter().find(|f| matches!(f.ty, FieldType::Vector { .. })).map(|f| f.name.clone()).ok_or_else(|| bad("the table has no VECTOR field".into()))?,
            };
            // Quoted: a field may be named like a keyword (`order`).
            let sql = format!("SELECT {} FROM {} WHERE {} = :k LIMIT 1", quoted(&field), ident(&s.table), quoted(&s.fields[pk].name));
            let rs = db.query(&sql, &Params::new().set("k", ingest::value(s, pk, &o["vector_of"])?))?;
            match rs.rows.into_iter().next().and_then(|r| r.into_iter().next()) {
                Some(v @ Value::Vector(_)) => v,
                Some(_) => return Err(bad(format!("row {} has no {field}", o["vector_of"]))),
                None => return Err(bad(format!("no row {}", o["vector_of"]))),
            }
        }
        Json::Object(m) if m.values().all(|w| w.is_number()) => Value::Str(j.to_string()),
        Json::Object(_) => return Err(bad("parameters are numbers, strings, null, lists, maps of term weights, {\"embed\": text} or {\"vector_of\": key}".into())),
    })
}

/// A row to write: its values as JSON, except the vectors of fields that
/// take one ([`ingest::takes_vector`]) given as `float[]`, which stay
/// float32.
struct Row {
    json: Json,
    vectors: Vec<(usize, Vec<f32>)>,
}

fn rows_of(rows: Vec<V>, schema: &Schema) -> R<Vec<Row>> {
    rows.into_iter().enumerate().map(|(i, r)| row_of(r, schema).map_err(|e| Fail::Arg(format!("row {}: {e}", i + 1)))).collect()
}

fn row_of(v: V, schema: &Schema) -> Result<Row, String> {
    let V::Map(kv) = v else { return Err(format!("a row is a map of field names to values, not {}", v.kind())) };
    let mut m = Map::new();
    let mut vectors = Vec::new();
    for (k, v) in kv {
        match v {
            V::Vector(xs) if schema.field_index(&k).is_some_and(|f| ingest::takes_vector(&schema.fields[f])) => vectors.push((schema.field_index(&k).expect("a field"), xs)),
            v => {
                m.insert(k, v.to_json()?);
            }
        }
    }
    Ok(Row { json: Json::Object(m), vectors })
}

/// Documents of `rows`, the first numbered `first` in errors.
fn documents(db: &Db, schema: &Schema, rows: Vec<Row>, first: usize) -> strata::Result<Vec<strata::Document>> {
    let now = strata::timestamp::now();
    let mut docs = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| ingest::document_with_vectors(schema, &r.json, r.vectors, now).map_err(|e| strata::Error::Schema(format!("row {}: {e}", first + i))))
        .collect::<strata::Result<Vec<_>>>()?;
    db.fill_in(&schema.table, &mut docs)?;
    Ok(docs)
}

/// A key as JSON, the form [`ingest::value`] reads.
fn key_json(v: &V) -> R<Json> {
    v.to_json().map_err(Fail::Arg)
}

/// The cores a database uses unless `threads` says otherwise: the machine's
/// (on Linux, its cgroup's quota).
fn threads_or_cores(threads: Option<usize>) -> R<usize> {
    match threads {
        Some(0) => arg("threads: at least 1"),
        Some(n) => Ok(n),
        None => Ok(std::thread::available_parallelism().map_or(1, |n| n.get())),
    }
}

/// How a database is opened here, as strata-server and the Python package
/// open one: segment builds (flushes, merges) build vector indexes on
/// `threads` cores, the newest in-memory chunk keeps its vectors flat,
/// and each write publishes its snapshot, so a query never waits to index
/// a memtable chunk another write filled.
fn db_options(directory: bool, threads: usize) -> DbOptions {
    DbOptions {
        publish_snapshots: true,
        layout: if directory { Layout::Directory } else { Layout::SingleFile },
        build: BuildOptions::default().set("vector.threads", threads),
        mem_build: Some(BuildOptions::default().set("vector.index", "flat")),
        memtable_max_docs: BULK_INSERT,
        ..DbOptions::default()
    }
}

/// Queries search segments on up to `threads` cores.
fn with_threads(mut db: Db, threads: usize) -> Db {
    db.exec.threads = threads;
    db
}

/// The schema of the table named `table` (any case), or of the first.
fn table_of(db: &Db, table: Option<&str>) -> strata::Result<Arc<Schema>> {
    match table {
        None => Ok(db.catalog().swap_remove(0)),
        Some(t) => db.storage().table(t).ok_or_else(|| strata::Error::Query(format!("no such table: {t}"))),
    }
}

/// One change of `alterTable`: `{"add": field}`, `{"drop": name}` or
/// `{"rename": [from, to]}`.
fn alter_of(j: &Json) -> R<strata::schema::Alter> {
    use strata::schema::{Alter, Field};
    let bad = || Fail::Arg(format!("a change is {{\"add\": field}}, {{\"drop\": name}} or {{\"rename\": [from, to]}}, not {j}"));
    let o = j.as_object().filter(|o| o.len() == 1).ok_or_else(bad)?;
    match o.iter().next().ok_or_else(bad)? {
        (k, f) if k == "add" => Ok(Alter::AddField(Field::from_short_json(f)?)),
        (k, Json::String(name)) if k == "drop" => Ok(Alter::DropField(name.clone())),
        (k, Json::Array(xs)) if k == "rename" => match xs.as_slice() {
            [Json::String(from), Json::String(to)] => Ok(Alter::RenameField { from: from.clone(), to: to.clone() }),
            _ => Err(bad()),
        },
        _ => Err(bad()),
    }
}

/// A file to read: the path given, or what an `s3://` URL names, read into
/// a temporary directory removed with this (feature `s3`).
struct Local {
    path: PathBuf,
    #[cfg(feature = "s3")]
    _held: Option<strata::objects::Input>,
}

fn local(path: &Path) -> strata::Result<Local> {
    #[cfg(feature = "s3")]
    if let Some(url) = path.to_str().filter(|s| strata::objects::is_url(s)) {
        let i = strata::objects::Input::open(url)?;
        return Ok(Local { path: i.path().to_path_buf(), _held: Some(i) });
    }
    Ok(Local {
        path: path.to_path_buf(),
        #[cfg(feature = "s3")]
        _held: None,
    })
}

/// A file or directory to write: the path given, or a temporary one that
/// [`Remote::finish`] writes to an `s3://` URL (feature `s3`).
struct Remote {
    path: PathBuf,
    #[cfg(feature = "s3")]
    out: Option<strata::objects::Output>,
}

fn remote(path: &Path) -> strata::Result<Remote> {
    #[cfg(feature = "s3")]
    if let Some(url) = path.to_str().filter(|s| strata::objects::is_url(s)) {
        let o = strata::objects::Output::new(url.trim_end_matches('/'))?;
        return Ok(Remote { path: o.path().to_path_buf(), out: Some(o) });
    }
    Ok(Remote {
        path: path.to_path_buf(),
        #[cfg(feature = "s3")]
        out: None,
    })
}

impl Remote {
    fn finish(self) -> strata::Result<()> {
        #[cfg(feature = "s3")]
        if let Some(o) = self.out {
            o.finish()?;
        }
        Ok(())
    }
}

fn sqlite_options(a: &mut Args) -> R<sqlite::ImportOptions> {
    let tables = a.names("tables")?.unwrap_or_default();
    let schemas = a.json_text("schemas")?.map(|s| sqlite::schemas_from_json(&s)).transpose()?.unwrap_or_default();
    let blobs_base64 = a.bool("blobsBase64", false)?;
    Ok(sqlite::ImportOptions { tables, schemas, blobs_base64 })
}

#[cfg(feature = "parquet")]
fn parquet_options(a: &mut Args) -> R<strata::parquet::ImportOptions> {
    let table = a.opt_str("table")?;
    let schema = a.json_text("schema")?.map(|s| Schema::from_json(&s)).transpose()?;
    let key = a.opt_str("key")?;
    let blobs_base64 = a.bool("blobsBase64", false)?;
    Ok(strata::parquet::ImportOptions { table, schema, key, blobs_base64 })
}

#[cfg(not(feature = "parquet"))]
fn no_parquet<T>() -> R<T> {
    arg("this build of Strata's native library has no Parquet (cargo feature parquet)")
}

fn train_options(a: &mut Args) -> R<TrainOptions> {
    let d = TrainOptions::default();
    Ok(TrainOptions { levels: a.count("levels", 3)?, k: a.count("k", 256)?, iters: a.count("iterations", 25)?, sample: a.count("sample", d.sample)?, seed: a.count("seed", 0)? as u64 })
}

/// A query's answer: columns, rows (lists), keys, scores and stats (JSON
/// text), and `extra` entries the caller writes after.
fn result(w: &mut Writer, rs: &ResultSet, extra: usize) {
    w.map(5 + extra);
    w.key("columns");
    w.list(rs.columns.len());
    rs.columns.iter().for_each(|c| w.str(c));
    w.key("rows");
    w.list(rs.rows.len());
    for r in &rs.rows {
        w.list(r.len());
        r.iter().for_each(|v| w.value(v));
    }
    w.key("keys");
    w.list(rs.keys.len());
    rs.keys.iter().for_each(|v| w.value(v));
    w.key("scores");
    w.list(rs.scores.len());
    rs.scores.iter().for_each(|&s| w.float(s));
    w.key("stats");
    w.str(&serde_json::to_string(&rs.stats).unwrap_or_default());
}

fn sql_result(r: SqlResult) -> Vec<u8> {
    let mut w = Writer::default();
    match r {
        SqlResult::Rows(rs) => result(&mut w, &rs, 0),
        SqlResult::Written(n) => {
            w.map(1);
            w.key("written");
            w.int(n as i64);
        }
    }
    w.b
}

fn json(j: &Json) -> Vec<u8> {
    let mut w = Writer::default();
    w.json(j);
    w.b
}

fn int(n: u64) -> Vec<u8> {
    let mut w = Writer::default();
    w.int(n as i64);
    w.b
}

fn null() -> Vec<u8> {
    vec![0]
}

/// Runs `method` with `args` on the database `h` (none for the calls that
/// open one, or need none); returns its answer.
pub fn call(h: Option<&Handle>, method: &str, args: V) -> R<Vec<u8>> {
    let mut a = Args::of(method, args)?;
    match h {
        None => call_static(method, &mut a),
        Some(h) => call_on(h, method, &mut a),
    }
}

fn call_static(method: &str, a: &mut Args) -> R<Vec<u8>> {
    Ok(match method {
        "version" => {
            let mut w = Writer::default();
            w.map(3);
            w.key("abi");
            w.int(ABI);
            w.key("version");
            w.str(env!("CARGO_PKG_VERSION"));
            w.key("features");
            let features: Vec<&str> = [("datafusion", cfg!(feature = "datafusion")), ("parquet", cfg!(feature = "parquet")), ("s3", cfg!(feature = "s3")), ("bert", cfg!(feature = "bert")), ("onnx", cfg!(feature = "onnx")), ("vision", cfg!(feature = "vision")), ("audio", cfg!(feature = "audio"))].into_iter().filter(|f| f.1).map(|f| f.0).collect();
            w.list(features.len());
            features.iter().for_each(|f| w.str(f));
            w.b
        }
        // The value given, as the native side read it: for the Java
        // package's tests of its values.
        "echo" => {
            let mut w = Writer::default();
            w.v(&a.take("value").unwrap_or(V::Null));
            w.b
        }
        "create" => {
            let path = a.path("path")?;
            let schema = a.json_text("schema")?.ok_or_else(|| Fail::Arg("create: schema is missing".into()))?;
            let schema = Schema::from_json(&schema)?;
            let directory = a.bool("directory", false)?;
            let models = a.opt_path("models")?.unwrap_or_else(strata::embed::models_dir);
            let threads = threads_or_cores(a.opt_count("threads")?)?;
            let db = Db::create_in(&path, schema, db_options(directory, threads), &models)?;
            Handle::new(with_threads(db, threads), &models).opened(None)
        }
        "open" => {
            let path = a.path("path")?;
            let models = a.opt_path("models")?.unwrap_or_else(strata::embed::models_dir);
            let threads = threads_or_cores(a.opt_count("threads")?)?;
            if !path.exists() {
                return Err(Fail::Db(strata::Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{} does not exist: Database.create makes one", path.display())))));
            }
            // A file's layout is its own: the option is a create's.
            let db = Db::open(&path, db_options(false, threads))?;
            Handle::new(with_threads(db, threads), &models).opened(None)
        }
        "fromSqlite" => {
            let sqlite_path = a.path("sqlitePath")?;
            let path = a.path("path")?;
            let opts = sqlite_options(a)?;
            let directory = a.bool("directory", false)?;
            let models = a.opt_path("models")?.unwrap_or_else(strata::embed::models_dir);
            let threads = threads_or_cores(a.opt_count("threads")?)?;
            let l = local(&sqlite_path)?;
            let src = sqlite::Source::open(&l.path)?;
            let plan = src.plan(None, &opts)?;
            let first = plan.tables.first().ok_or_else(|| strata::Error::Schema(format!("{}: no table to import", sqlite_path.display())))?;
            let db = Db::create_in(&path, first.schema.clone(), db_options(directory, threads), &models)?;
            // Before the rows, so that they get their embeddings.
            let h = Handle::new(with_threads(db, threads), &models);
            let report = src.load(&*h.get()?, &plan)?.to_json();
            h.opened(Some(report))
        }
        "planSqlite" => {
            let sqlite_path = a.path("sqlitePath")?;
            let opts = sqlite_options(a)?;
            json(&sqlite::plan(&local(&sqlite_path)?.path, None, &opts)?.to_json())
        }
        #[cfg(feature = "parquet")]
        "fromParquet" => {
            let parquet_path = a.path("parquetPath")?;
            let path = a.path("path")?;
            let opts = parquet_options(a)?;
            let directory = a.bool("directory", false)?;
            let models = a.opt_path("models")?.unwrap_or_else(strata::embed::models_dir);
            let threads = threads_or_cores(a.opt_count("threads")?)?;
            let l = local(&parquet_path)?;
            let src = strata::parquet::Source::open(&l.path)?;
            let plan = src.plan(None, &opts)?;
            let db = Db::create_in(&path, plan.schema.clone(), db_options(directory, threads), &models)?;
            let h = Handle::new(with_threads(db, threads), &models);
            let report = src.load(&*h.get()?, &plan)?.to_json();
            h.opened(Some(report))
        }
        #[cfg(feature = "parquet")]
        "planParquet" => {
            let parquet_path = a.path("parquetPath")?;
            let opts = parquet_options(a)?;
            json(&strata::parquet::plan(&local(&parquet_path)?.path, None, &opts)?.to_json())
        }
        #[cfg(not(feature = "parquet"))]
        "fromParquet" | "planParquet" => no_parquet()?,
        "upgrade" => {
            let path = a.path("path")?;
            match strata::upgrade::upgrade(&path)? {
                None => null(),
                Some(u) => json(&serde_json::json!({"old": u.old.to_string_lossy(), "tables": u.tables, "rows": u.rows})),
            }
        }
        other => return arg(format!("no such call: {other}")),
    })
}

fn call_on(h: &Handle, method: &str, a: &mut Args) -> R<Vec<u8>> {
    if method == "close" {
        // Merges small segments first, as a server does when it stops, and
        // returns the space merges freed to the file system when it is an
        // eighth of the file or more.
        let db = h.db.write().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(db) = db {
            db.close()?;
        }
        return Ok(null());
    }
    if method == "closed" {
        let mut w = Writer::default();
        w.bool(h.db.read().unwrap_or_else(|e| e.into_inner()).is_none());
        return Ok(w.b);
    }
    let db = h.get()?;
    let db = &*db;
    Ok(match method {
        "embedErrors" => {
            let mut w = Writer::default();
            w.map(h.embed_errors.len());
            for (f, e) in &h.embed_errors {
                w.key(f);
                w.str(e);
            }
            w.b
        }
        "schema" => {
            let table = a.opt_str("table")?;
            json(&table_of(db, table.as_deref())?.to_short_json())
        }
        "tables" => json(&Json::Array(db.catalog().iter().map(|s| s.to_short_json()).collect())),
        "alterTable" => {
            let table = a.opt_str("table")?;
            let table = table_of(db, table.as_deref())?.table.clone();
            let changes = match a.json("changes")? {
                Some(Json::Array(xs)) => xs,
                Some(j @ Json::Object(_)) => vec![j],
                _ => return arg("alterTable: changes is a list of {\"add\": field}, {\"drop\": name} and {\"rename\": [from, to]}"),
            };
            let alters = changes.iter().map(alter_of).collect::<R<Vec<_>>>()?;
            db.alter_table(&table, &alters)?;
            null()
        }
        "count" => int(db.snapshot().n_live_docs()),
        "query" => {
            let sql = a.str("sql")?;
            let p = a.params(db)?;
            // EXPLAIN answers rows too, and writes nothing.
            let explain = sql.trim_start().get(..7).is_some_and(|w| w.eq_ignore_ascii_case("explain"));
            match explain {
                true => sql_result(db.sql(&sql, &p)?),
                false => {
                    let mut w = Writer::default();
                    result(&mut w, &db.query(&sql, &p)?, 0);
                    w.b
                }
            }
        }
        "sql" => {
            let sql = a.str("sql")?;
            let p = a.params(db)?;
            sql_result(db.sql(&sql, &p)?)
        }
        "queryLogged" => {
            let sql = a.str("sql")?;
            let p = a.params(db)?;
            let log = a.json("log")?.unwrap_or_else(|| Json::Object(Map::new()));
            let l = db.query_logged(&sql, &p, &Log::from_json(&log)?)?;
            let mut w = Writer::default();
            result(&mut w, &l.rs, 1 + !l.teams.is_empty() as usize);
            w.key("request");
            w.str(&l.request);
            if !l.teams.is_empty() {
                w.key("teams");
                w.list(l.teams.len());
                for t in &l.teams {
                    match t {
                        Some(t) => w.str(t),
                        None => w.null(),
                    }
                }
            }
            w.b
        }
        "exportRankLog" => {
            let x = a.json("options")?.unwrap_or_else(|| Json::Object(Map::new()));
            let x = Export::from_json(&x)?;
            let mut out = Vec::new();
            db.export_rank_log(&x, |r| {
                out.push(r);
                Ok(())
            })?;
            json(&Json::Array(out))
        }
        "plan" => {
            let sql = a.str("sql")?;
            let p = a.params(db)?;
            let text = db.explain(&sql, &p)?.to_json_text()?;
            json(&serde_json::from_str(&text).map_err(|e| strata::Error::Query(format!("a plan's JSON: {e}")))?)
        }
        "run" => {
            let plan = a.json("plan")?.ok_or_else(|| Fail::Arg("run: plan is missing".into()))?;
            sql_result(db.run(&Plan::from_json(&plan)?)?)
        }
        "insert" => {
            let table = a.opt_str("table")?;
            let s = table_of(db, table.as_deref())?;
            let rows = rows_of(a.list("rows")?, &s)?;
            let docs = documents(db, &s, rows, 1)?;
            let n = docs.len() as u64;
            if docs.len() >= BULK_INSERT {
                db.load_into(&s.table, docs)?;
            } else {
                // Threads inserting at once share commits.
                db.write_ops(docs.into_iter().map(|doc| WriteOp::Insert { table: s.table.clone(), doc }).collect())?;
            }
            int(n)
        }
        "upsert" => {
            let table = a.opt_str("table")?;
            let s = table_of(db, table.as_deref())?;
            let rows = rows_of(a.list("rows")?, &s)?;
            let mut n = 0;
            let mut rows = rows.into_iter().peekable();
            while rows.peek().is_some() {
                let chunk: Vec<Row> = rows.by_ref().take(BATCH).collect();
                let len = chunk.len() as u64;
                let docs = documents(db, &s, chunk, n as usize + 1)?;
                db.write_ops(docs.into_iter().map(|doc| WriteOp::Upsert { table: s.table.clone(), doc }).collect())?;
                n += len;
            }
            int(n)
        }
        "delete" => {
            let table = a.opt_str("table")?;
            let keys = a.list("keys")?;
            let s = table_of(db, table.as_deref())?;
            let pk = s.primary_key.ok_or_else(|| strata::Error::Query("deleting by key needs a primary key".into()))?;
            let ops = keys.iter().map(|k| Ok(WriteOp::Delete { table: s.table.clone(), key: ingest::value(&s, pk, &key_json(k)?)? })).collect::<R<Vec<_>>>()?;
            int(db.write_ops(ops)?)
        }
        "getMany" => {
            let table = a.opt_str("table")?;
            let keys = a.list("keys")?;
            let s = table_of(db, table.as_deref())?;
            let pk = s.primary_key.ok_or_else(|| strata::Error::Query(format!("{} has no primary key: rows are read by key only from a table with one", s.table)))?;
            let keys = keys.iter().map(|k| Ok(ingest::value(&s, pk, &key_json(k)?)?)).collect::<R<Vec<_>>>()?;
            let columns = Db::get_columns(&s);
            let rows = db.get_many(&s.table, &keys)?;
            let mut w = Writer::default();
            w.list(rows.len());
            for r in &rows {
                match r {
                    None => w.null(),
                    Some(row) => {
                        w.map(columns.len().min(row.len()));
                        for (c, v) in columns.iter().zip(row) {
                            w.key(c);
                            w.value(v);
                        }
                    }
                }
            }
            w.b
        }
        "expire" => int(db.expire(strata::timestamp::now(), 10_000)?),
        "write" => int(write(db, a.list("ops")?)?),
        "importFile" => {
            let path = a.path("path")?;
            let format = a.opt_str("format")?;
            let format = format.map(|f| Format::parse(&f).ok_or_else(|| Fail::Arg(format!("unknown format {f:?}: jsonl, json, csv or tsv")))).transpose()?;
            let table = a.opt_str("table")?;
            let s = table_of(db, table.as_deref())?;
            let r = ingest::import_file_into(db, &s.table, &local(&path)?.path, format)?;
            json(&serde_json::json!({"rows": r.rows, "bulk": r.bulk, "skipped": r.unknown}))
        }
        "importSqlite" => {
            let path = a.path("path")?;
            let opts = sqlite_options(a)?;
            json(&sqlite::import(db, &local(&path)?.path, &opts)?.to_json())
        }
        "exportSqlite" => {
            let path = a.path("path")?;
            let opts = sqlite::ExportOptions { tables: a.names("tables")?.unwrap_or_default(), fts: a.bool("fts", true)?, indexes: a.bool("indexes", true)?, meta: a.bool("schemas", true)? };
            let out = remote(&path)?;
            let r = sqlite::export(db, &out.path, &opts)?;
            out.finish()?;
            json(&r.to_json())
        }
        #[cfg(feature = "parquet")]
        "importParquet" => {
            let path = a.path("path")?;
            let opts = parquet_options(a)?;
            json(&strata::parquet::import(db, &local(&path)?.path, &opts)?.to_json())
        }
        #[cfg(feature = "parquet")]
        "exportParquet" => {
            let path = a.path("path")?;
            let opts = strata::parquet::ExportOptions { tables: a.names("tables")?.unwrap_or_default(), meta: a.bool("schemas", true)? };
            let out = remote(&path)?;
            let r = strata::parquet::export(db, &out.path, &opts)?;
            out.finish()?;
            json(&r.to_json())
        }
        #[cfg(not(feature = "parquet"))]
        "importParquet" | "exportParquet" => no_parquet()?,
        "embed" => {
            let text = a.opt_str("text")?;
            let field = a.opt_str("field")?;
            let table = a.opt_str("table")?;
            let image = a.bytes("image")?;
            let audio = a.bytes("audio")?;
            let mut w = Writer::default();
            w.vector(&embed(db, text, field, table, image, audio)?);
            w.b
        }
        "embedTokens" => {
            let text = a.str("text")?;
            let field = a.str("field")?;
            let table = a.opt_str("table")?;
            let table = table_of(db, table.as_deref())?.table.clone();
            match db.embed_query_in(&table, &field, &text)? {
                Value::List(vs) => {
                    let mut w = Writer::default();
                    let vs: Vec<_> = vs.iter().filter_map(|v| if let Value::Vector(v) = v { Some(v) } else { None }).collect();
                    w.list(vs.len());
                    vs.iter().for_each(|v| w.vector(v));
                    w.b
                }
                _ => return arg(format!("{field} is a field of one vector: Database.embed gives it")),
            }
        }
        "sidNext" => {
            let field = a.str("field")?;
            let prefixes = a.names("prefixes")?.unwrap_or_default();
            let filter = a.opt_str("where")?;
            let p = a.params(db)?;
            let table = a.opt_str("table")?;
            let table = table_of(db, table.as_deref())?.table.clone();
            let next = db.sid_next(&table, &field, &prefixes, filter.as_deref(), &p)?;
            let mut w = Writer::default();
            w.map(prefixes.len());
            for (prefix, codes) in prefixes.iter().zip(&next) {
                w.key(prefix);
                w.map(codes.len());
                for (code, n) in codes {
                    w.key(code);
                    w.int(*n as i64);
                }
            }
            w.b
        }
        "trainCodebook" => {
            let field = a.str("field")?;
            let name = a.str("name")?;
            let table = a.opt_str("table")?;
            let table = table_of(db, table.as_deref())?.table.clone();
            let models = a.opt_path("models")?.unwrap_or_else(strata::embed::models_dir);
            let opts = train_options(a)?;
            let mut w = Writer::default();
            w.str(&db.train_codebook(&table, &field, &name, &models, &opts)?);
            w.b
        }
        // As strata-py's: the memtable written out, segments merged into
        // as few as can be, the space freed returned to the file system.
        "optimize" => {
            db.optimize()?;
            null()
        }
        other => return arg(format!("no such call: {other}")),
    })
}

/// The vector `field`'s model gives `text` as a query, or `image` or
/// `audio`, the bytes of an image or sound file, for a model of that.
fn embed(db: &Db, text: Option<String>, field: Option<String>, table: Option<String>, image: Option<Vec<u8>>, audio: Option<Vec<u8>>) -> R<Vec<f32>> {
    use strata::embed::Modality;
    let file = match (image, audio) {
        (Some(_), Some(_)) => return arg("embed takes a text, an image or a sound, one of them"),
        (Some(d), None) => Some((Modality::Image, d)),
        (None, Some(d)) => Some((Modality::Audio, d)),
        (None, None) => None,
    };
    if let Some((m, data)) = file {
        let s = table_of(db, table.as_deref())?;
        let what = strata::embed::modality_name(m);
        let field = match field {
            Some(f) => f,
            None => s.fields.iter().find(|f| db.embedder_in(&s.table, &f.name).is_some_and(|e| e.modalities().contains(&m))).map(|f| f.name.clone()).ok_or_else(|| Fail::Arg(format!("no embedded field has a model of {what} open")))?,
        };
        let e = db.embedder_in(&s.table, &field).ok_or_else(|| Fail::Arg(format!("{field} has no embedding model open (see Database.Options.models)")))?;
        strata::embed::file_is(m, &data)?;
        return Ok(strata::embed::embed_file(e.as_ref(), &data)?);
    }
    let text = text.ok_or_else(|| Fail::Arg("embed takes a text, an image or a sound".into()))?;
    let mut o = Map::new();
    o.insert("embed".into(), Json::String(text));
    if let Some(f) = field {
        o.insert("field".into(), Json::String(f));
    }
    if let Some(t) = table {
        o.insert("table".into(), Json::String(t));
    }
    match param(db, &Json::Object(o))? {
        Value::Vector(v) => Ok(v.to_vec()),
        _ => arg("the field's model embeds a text a vector a token (a multi-vector field): Database.embedTokens gives them"),
    }
}

/// Applies write operations as strata-server's /v1/write takes them, in
/// one transaction: insert, upsert and delete, and set and add of a numeric
/// field in place (an add with a "doc" inserts it when no row has its
/// key). Returns the rows written.
fn write(db: &Db, ops: Vec<V>) -> R<u64> {
    let ops = ops.iter().map(|o| o.to_json().map_err(Fail::Arg)).collect::<R<Vec<_>>>()?;
    let bad = |i: usize, m: &str| strata::Error::Query(format!("operation {}: {m}", i + 1));
    // Read before the write starts: threads writing at once share its
    // commit.
    let mut write = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        let get = |k: &str| op.get(k).filter(|v| !v.is_null());
        let kind = get("op").and_then(Json::as_str).ok_or_else(|| bad(i, "it needs \"op\""))?;
        let s = table_of(db, get("table").and_then(Json::as_str))?;
        let table = s.table.clone();
        let doc = || match get("doc") {
            Some(d) => Ok(documents(db, &s, vec![Row { json: d.clone(), vectors: Vec::new() }], 1)?.remove(0)),
            None => Err(bad(i, &format!("{kind} needs \"doc\""))),
        };
        let key = || {
            let pk = s.primary_key.ok_or_else(|| bad(i, "writing by key needs a primary key"))?;
            ingest::value(&s, pk, get("key").ok_or_else(|| bad(i, &format!("{kind} needs \"key\"")))?)
        };
        let field = || get("field").and_then(Json::as_str).map(str::to_string).ok_or_else(|| bad(i, &format!("{kind} needs \"field\"")));
        write.push(match kind {
            "insert" => WriteOp::Insert { table, doc: doc()? },
            "upsert" => WriteOp::Upsert { table, doc: doc()? },
            "delete" => WriteOp::Delete { table, key: key()? },
            "set" => {
                let f = field()?;
                let fi = s.field_index(&f).ok_or_else(|| bad(i, &format!("no field {f}")))?;
                WriteOp::Set { table, key: key()?, value: ingest::value(&s, fi, get("value").unwrap_or(&Json::Null))?, field: f }
            }
            "add" => {
                let delta = match get("value") {
                    Some(Json::Number(x)) => x.as_i64().map(Value::Int).or_else(|| x.as_f64().map(Value::Float)),
                    _ => None,
                };
                let delta = delta.ok_or_else(|| bad(i, "add needs a numeric value"))?;
                if get("doc").is_some() {
                    let d = doc()?;
                    if get("key").is_some() && Some(&key()?) != s.primary_key.map(|pk| &d[pk]) {
                        return Err(bad(i, "add's key and its doc's key differ: give one").into());
                    }
                    WriteOp::AddOrInsert { table, doc: d, field: field()?, delta }
                } else {
                    WriteOp::Add { table, key: key()?, field: field()?, delta }
                }
            }
            other => return Err(bad(i, &format!("unknown op {other:?}")).into()),
        });
    }
    Ok(db.write_ops(write)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::decode;

    fn m(kv: &[(&str, V)]) -> V {
        V::Map(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    fn s(x: &str) -> V {
        V::Str(x.into())
    }

    fn get<'a>(v: &'a V, k: &str) -> &'a V {
        match v {
            V::Map(kv) => &kv.iter().find(|(n, _)| n == k).unwrap_or_else(|| panic!("no {k} in {v:?}")).1,
            _ => panic!("not a map: {v:?}"),
        }
    }

    /// A temporary directory, removed when the test ends.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let p = std::env::temp_dir().join(format!("strata-java-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn file(&self, f: &str) -> V {
            s(self.0.join(f).to_str().unwrap())
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open(args: V, method: &str) -> &'static Handle {
        let out = decode(&call(None, method, args).unwrap()).unwrap();
        let V::Int(h) = get(&out, "handle") else { panic!() };
        // The tests leak their handles, as Java would until it frees them.
        unsafe { &*(*h as *const Handle) }
    }

    fn schema() -> V {
        s(r#"{"table": "books", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text"},
            {"name": "year", "type": "int"},
            {"name": "v", "type": "vector", "dim": 2}]}"#)
    }

    #[test]
    fn rows_go_in_and_come_out() {
        let d = Dir::new("rows");
        let h = open(m(&[("path", d.file("b.strata")), ("schema", schema()), ("threads", V::Int(1))]), "create");
        let rows = V::List(vec![
            m(&[("id", s("b1")), ("title", s("The river and the dragon")), ("year", V::Int(1999)), ("v", V::Vector(vec![1.0, 0.0]))]),
            m(&[("id", s("b2")), ("title", s("A mountain")), ("year", V::Int(2004)), ("v", V::Vector(vec![0.0, 1.0]))]),
        ]);
        assert_eq!(decode(&call(Some(h), "insert", m(&[("rows", rows)])).unwrap()).unwrap(), V::Int(2));
        let q = m(&[("sql", s("SELECT id, year FROM books WHERE year > :y ORDER BY bm25(title, :q) DESC LIMIT 5")), ("params", m(&[("y", V::Int(1990)), ("q", s("dragon"))]))]);
        let r = decode(&call(Some(h), "query", q).unwrap()).unwrap();
        assert_eq!(get(&r, "columns"), &V::List(vec![s("id"), s("year")]));
        assert_eq!(get(&r, "rows"), &V::List(vec![V::List(vec![s("b1"), V::Int(1999)])]));
        // A vector parameter as a float[].
        let q = m(&[("sql", s("SELECT id FROM books ORDER BY similarity(v, :v) DESC LIMIT 1")), ("params", m(&[("v", V::Vector(vec![0.1, 0.9]))]))]);
        assert_eq!(get(&decode(&call(Some(h), "query", q).unwrap()).unwrap(), "rows"), &V::List(vec![V::List(vec![s("b2")])]));
        let w = decode(&call(Some(h), "sql", m(&[("sql", s("UPDATE books SET year = 2000 WHERE id = 'b1'"))])).unwrap()).unwrap();
        assert_eq!(get(&w, "written"), &V::Int(1));
        let got = decode(&call(Some(h), "getMany", m(&[("keys", V::List(vec![s("b1"), s("nope")]))])).unwrap()).unwrap();
        let V::List(got) = got else { panic!() };
        assert_eq!(get(&got[0], "year"), &V::Int(2000));
        assert_eq!(got[1], V::Null);
        assert_eq!(decode(&call(Some(h), "delete", m(&[("keys", V::List(vec![s("b2")]))])).unwrap()).unwrap(), V::Int(1));
        assert_eq!(decode(&call(Some(h), "count", V::Null).unwrap()).unwrap(), V::Int(1));
        call(Some(h), "close", V::Null).unwrap();
        assert!(matches!(call(Some(h), "count", V::Null), Err(Fail::Closed)));
        // And again, from the file.
        let h = open(m(&[("path", d.file("b.strata"))]), "open");
        assert_eq!(decode(&call(Some(h), "count", V::Null).unwrap()).unwrap(), V::Int(1));
        let sch = decode(&call(Some(h), "schema", V::Null).unwrap()).unwrap();
        assert_eq!(get(&sch, "table"), &s("books"));
        call(Some(h), "close", V::Null).unwrap();
    }

    #[test]
    fn wrong_arguments_say_what_is_taken() {
        let d = Dir::new("args");
        let h = open(m(&[("path", d.file("a.strata")), ("schema", schema())]), "create");
        let e = call(Some(h), "query", m(&[("sql", s("SELECT 1")), ("limit", V::Int(3))])).unwrap_err();
        let Fail::Arg(e) = e else { panic!("{e:?}") };
        assert!(e.contains("no option \"limit\"") && e.contains("sql, params"), "{e}");
        let Fail::Arg(e) = call(Some(h), "query", m(&[("sql", V::Int(1))])).unwrap_err() else { panic!() };
        assert!(e.contains("sql is a string, not an integer"), "{e}");
        assert!(matches!(call(Some(h), "query", m(&[("sql", s("SELECT nope FROM books"))])), Err(Fail::Db(_))));
        let Fail::Arg(e) = call(Some(h), "insert", m(&[("rows", V::List(vec![V::Int(1)]))])).unwrap_err() else { panic!() };
        assert!(e.starts_with("row 1:"), "{e}");
        assert!(matches!(call(None, "nope", V::Null), Err(Fail::Arg(_))));
        call(Some(h), "close", V::Null).unwrap();
    }
}
