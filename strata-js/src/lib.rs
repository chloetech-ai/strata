//! JavaScript bindings for Strata, the database in-process: a Node-API
//! addon (`strata.node`), which `index.js` wraps in the documented API.
//! Node, Bun and Deno load it.
//!
//! The boundary is narrow on purpose. A database answers one entry point,
//! [`NativeDatabase::call`], which takes an operation's name and its
//! arguments as JSON text and gives its answer as JSON text: V8 parses and
//! writes JSON far faster than an addon builds objects a property at a
//! time, and `index.js` and `client.js` (strata-server over HTTP) read the
//! same answers. Vectors of rows can travel apart from the JSON as
//! `Float32Array`s, read from their bytes, as strata-py reads numpy arrays.
//!
//! Every call runs on libuv's thread pool and answers a promise, so queries
//! and writes leave the event loop free and run at once from many callers;
//! [`NativeDatabase::call_sync`] runs one on the calling thread.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use napi::bindgen_prelude::{AsyncTask, Float32Array};
use napi::{Env, Task};
use napi_derive::napi;
use serde_json::{json, Map, Value as Json};
use strata::db::{Db, SqlResult, WriteOp};
use strata::ingest::{self, Format};
use strata::query::exec::ResultSet;
use strata::query::ir::Plan;
use strata::query::sql::{ident, quoted};
use strata::query::Params;
use strata::rank_log::{Export, Log};
use strata::schema::Schema;
use strata::segment::build::BuildOptions;
use strata::sid::{Codebook, TrainOptions};
use strata::sqlite;
use strata::storage::{DbOptions, Layout, MergerOptions};
use strata::{Error, FieldType, Result, Value};

/// Rows a write transaction of an upsert carries (as `strata-server
/// import` does).
const BATCH: usize = ingest::IMPORT_BATCH;
/// Inserts of at least this many rows, the memtable's size, are bulk-loaded
/// into new segments in one commit; fewer go through the write-ahead log
/// into the memtable, which gathers many inserts into one segment (as
/// strata-py does).
const BULK_INSERT: usize = 10_000;

fn napi_err(e: Error) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

fn bad(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

// ---------------------------------------------------------------- values

/// A stored value as JSON, as strata-server answers it: a TIMESTAMP as
/// ISO 8601 text in UTC, a float that is not finite as null.
fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Int(x) => json!(x),
        Value::Float(x) if x.is_finite() => json!(x),
        Value::Float(_) => Json::Null,
        Value::Str(s) => json!(s),
        Value::Vector(v) => Json::Array(v.iter().map(|x| json!(x)).collect()),
        Value::List(xs) => Json::Array(xs.iter().map(to_json).collect()),
        Value::Bool(b) => json!(b),
        Value::Timestamp(t) => json!(strata::timestamp::format(*t)),
    }
}

/// A query's answer as strata-server's /v1/query gives it: columns, rows
/// (lists), keys, scores and stats.
fn result_json(rs: &ResultSet) -> Json {
    json!({
        "columns": rs.columns,
        "rows": rs.rows.iter().map(|r| r.iter().map(to_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "keys": rs.keys.iter().map(to_json).collect::<Vec<_>>(),
        "scores": rs.scores.iter().map(|s| if s.is_finite() { json!(s) } else { Json::Null }).collect::<Vec<_>>(),
        "stats": rs.stats,
    })
}

/// A statement's answer: a read's rows, or `{"written": n}`, as
/// strata-server's /v1/sql gives them.
fn sql_json(r: SqlResult) -> Json {
    match r {
        SqlResult::Rows(rs) => result_json(&rs),
        SqlResult::Written(n) => json!({ "written": n }),
    }
}

/// A query parameter, as strata-py and the HTTP API read one: a number,
/// string, null, a list of numbers (a vector), a list, an object of term
/// weights (SPARSE), `{"embed": text, "field": f}` (the text's vector by
/// `f`'s model, or an image's or a sound's for `{"embed": {"image":
/// "<base64>"}}` or `{"embed": {"audio": "<base64>"}}`) or `{"vector_of":
/// key, "field": f}` (a stored row's vector), each with `"table"` for a
/// field of another table than the first.
fn param(db: &Db, j: &Json) -> Result<Value> {
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
        Json::Array(xs) => Value::List(xs.iter().map(|x| param(db, x)).collect::<Result<_>>()?),
        Json::Object(o) if o.get("embed").is_some_and(|e| e.is_string() || e.get("image").is_some() || e.get("audio").is_some()) && o.keys().all(|k| k == "embed" || k == "field" || k == "table") => {
            let s = table_of(db, o.get("table").and_then(|t| t.as_str()))?;
            let field = match o.get("field").and_then(|f| f.as_str()) {
                Some(f) => f.to_string(),
                None => s.fields.iter().find(|f| db.embedder_in(&s.table, &f.name).is_some()).map(|f| f.name.clone()).ok_or_else(|| bad("no embedded field has its model open"))?,
            };
            let e = db.embedder_in(&s.table, &field).ok_or_else(|| bad(format!("{field} has no embedding model open (see open's models)")))?;
            match &o["embed"] {
                // A multi-vector field's: the list of the tokens' vectors.
                Json::String(t) => db.embed_query_in(&s.table, &field, t)?,
                Json::Object(i) if i.len() == 1 && i.values().all(|v| v.is_string()) => {
                    let (kind, b64) = i.iter().next().expect("one key");
                    let (_, data) = strata::embed::query_file(kind, b64.as_str().unwrap_or_default())?;
                    Value::Vector(strata::embed::embed_file(e.as_ref(), &data)?.into())
                }
                _ => return Err(bad("\"embed\" is a text, {\"image\": \"<base64>\"} or {\"audio\": \"<base64>\"}")),
            }
        }
        Json::Object(o) if o.contains_key("vector_of") && o.keys().all(|k| k == "vector_of" || k == "field" || k == "table") => {
            let s = &*table_of(db, o.get("table").and_then(|t| t.as_str()))?;
            let pk = s.primary_key.ok_or_else(|| bad("vector_of needs a primary key"))?;
            let field = match o.get("field").and_then(|f| f.as_str()) {
                Some(f) => f.to_string(),
                None => s.fields.iter().find(|f| matches!(f.ty, FieldType::Vector { .. })).map(|f| f.name.clone()).ok_or_else(|| bad("the table has no VECTOR field"))?,
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
        Json::Object(_) => return Err(bad("parameters are numbers, strings, null, arrays, objects of term weights, {\"embed\": text} or {\"vector_of\": key}")),
    })
}

fn params(db: &Db, j: Option<&Json>) -> Result<Params> {
    let mut p = Params::new();
    match j {
        None | Some(Json::Null) => {}
        Some(Json::Object(m)) => {
            for (k, v) in m {
                p = p.set(k, param(db, v)?);
            }
        }
        Some(_) => return Err(bad("params are an object of names to values")),
    }
    Ok(p)
}

// ---------------------------------------------------------------- arguments

/// An operation's arguments: a JSON object.
struct Args(Map<String, Json>);

impl Args {
    fn parse(text: &str) -> Result<Args> {
        match serde_json::from_str(if text.is_empty() { "{}" } else { text }) {
            Ok(Json::Object(m)) => Ok(Args(m)),
            Ok(_) => Err(bad("the arguments are a JSON object")),
            Err(e) => Err(bad(format!("the arguments are JSON: {e}"))),
        }
    }

    /// The argument `k`; null is no argument.
    fn get(&self, k: &str) -> Option<&Json> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    fn take(&mut self, k: &str) -> Option<Json> {
        self.0.remove(k).filter(|v| !v.is_null())
    }

    fn str(&self, k: &str) -> Result<Option<&str>> {
        match self.get(k) {
            None => Ok(None),
            Some(Json::String(s)) => Ok(Some(s)),
            Some(v) => Err(bad(format!("{k} is a string, not {v}"))),
        }
    }

    fn need_str(&self, k: &str) -> Result<&str> {
        self.str(k)?.ok_or_else(|| bad(format!("{k} is missing")))
    }

    fn path(&self, k: &str) -> Result<Option<PathBuf>> {
        Ok(self.str(k)?.map(PathBuf::from))
    }

    fn need_path(&self, k: &str) -> Result<PathBuf> {
        Ok(PathBuf::from(self.need_str(k)?))
    }

    fn bool(&self, k: &str, default: bool) -> Result<bool> {
        match self.get(k) {
            None => Ok(default),
            Some(Json::Bool(b)) => Ok(*b),
            Some(v) => Err(bad(format!("{k} is true or false, not {v}"))),
        }
    }

    fn uint(&self, k: &str) -> Result<Option<u64>> {
        match self.get(k) {
            None => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(|| bad(format!("{k} is a whole number, not {v}"))),
        }
    }

    fn strings(&self, k: &str) -> Result<Option<Vec<String>>> {
        match self.get(k) {
            None => Ok(None),
            Some(Json::String(s)) => Ok(Some(vec![s.clone()])),
            Some(Json::Array(xs)) => xs.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| bad(format!("{k} are strings, not {x}")))).collect::<Result<_>>().map(Some),
            Some(v) => Err(bad(format!("{k} are strings, not {v}"))),
        }
    }

    /// An argument given as JSON text or as the value itself (a schema, a
    /// plan), as text.
    fn text(&self, k: &str) -> Option<String> {
        self.get(k).map(|v| match v {
            Json::String(s) => s.clone(),
            v => v.to_string(),
        })
    }

    /// A list argument: a list, or one value given alone.
    fn list(&mut self, k: &str) -> Vec<Json> {
        match self.take(k) {
            None => Vec::new(),
            Some(Json::Array(xs)) => xs,
            Some(v) => vec![v],
        }
    }
}

// ---------------------------------------------------------------- opening

/// The cores a database uses unless `threads` says otherwise: the machine's
/// (on Linux, its cgroup's quota).
fn threads_or_cores(threads: Option<u64>) -> Result<usize> {
    match threads {
        Some(0) => Err(bad("threads: at least 1")),
        Some(n) => Ok(n as usize),
        None => Ok(std::thread::available_parallelism().map_or(1, |n| n.get())),
    }
}

/// How this module opens a database, as strata-server and strata-py do:
/// segment builds (flushes, merges) build vector indexes on `threads`
/// cores, the newest in-memory chunk keeps its vectors flat, and each write
/// publishes its snapshot, so a query never waits to index a memtable
/// chunk another write filled.
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

fn models_of(a: &Args) -> Result<PathBuf> {
    Ok(a.path("models")?.unwrap_or_else(strata::embed::models_dir))
}

fn embed_errors_of(db: &Db, models: &Path) -> Vec<(String, String)> {
    db.open_embedders(models).into_iter().map(|(f, e)| (f, e.to_string())).collect()
}

fn sqlite_options(a: &Args) -> Result<sqlite::ImportOptions> {
    let schemas = a.text("schemas").map(|s| sqlite::schemas_from_json(&s)).transpose()?.unwrap_or_default();
    Ok(sqlite::ImportOptions { tables: a.strings("tables")?.unwrap_or_default(), schemas, blobs_base64: a.bool("blobs_base64", false)? })
}

#[cfg(feature = "parquet")]
fn parquet_options(a: &Args) -> Result<strata::parquet::ImportOptions> {
    let schema = a.text("schema").map(|s| Schema::from_json(&s)).transpose()?;
    Ok(strata::parquet::ImportOptions { table: a.str("table")?.map(String::from), schema, key: a.str("key")?.map(String::from), blobs_base64: a.bool("blobs_base64", false)? })
}

/// A database opened, with what its opening says: the embedded fields
/// whose model did not open and, after an import, what it did.
pub struct Opened {
    db: Db,
    embed_errors: Vec<(String, String)>,
    last_import: Option<String>,
}

/// Opens or creates a database: `op` is "create" ({path, schema,
/// directory?, models?, threads?}), "open" ({path, models?, threads?}),
/// "from_sqlite" ({sqlite_path, path, tables?, schemas?, blobs_base64?,
/// directory?, models?, threads?}) or "from_parquet" ({parquet_path, path,
/// table?, schema?, key?, blobs_base64?, directory?, models?, threads?}).
fn open_db(op: &str, a: &Args) -> Result<Opened> {
    let threads = threads_or_cores(a.uint("threads")?)?;
    let models = models_of(a)?;
    let path = a.need_path("path")?;
    let directory = a.bool("directory", false)?;
    let (db, embed_errors, last_import) = match op {
        "create" => {
            let schema = Schema::from_json(&a.text("schema").ok_or_else(|| bad("schema is missing"))?)?;
            let db = Db::create_in(&path, schema, db_options(directory, threads), &models)?;
            let e = embed_errors_of(&db, &models);
            (db, e, None)
        }
        // The layout comes from the file; the option only matters to a create.
        "open" => {
            let db = Db::open(&path, db_options(false, threads))?;
            let e = embed_errors_of(&db, &models);
            (db, e, None)
        }
        "from_sqlite" => {
            let opts = sqlite_options(a)?;
            let src_path = a.need_path("sqlite_path")?;
            let l = local(&src_path)?;
            let src = sqlite::Source::open(&l.path)?;
            let plan = src.plan(None, &opts)?;
            let first = plan.tables.first().ok_or_else(|| Error::Schema(format!("{}: no table to import", src_path.display())))?;
            let db = Db::create_in(&path, first.schema.clone(), db_options(directory, threads), &models)?;
            // Before the rows, so that they get their embeddings.
            let e = embed_errors_of(&db, &models);
            let r = src.load(&db, &plan)?;
            (db, e, Some(r.to_json().to_string()))
        }
        #[cfg(feature = "parquet")]
        "from_parquet" => {
            let opts = parquet_options(a)?;
            let l = local(&a.need_path("parquet_path")?)?;
            let src = strata::parquet::Source::open(&l.path)?;
            let plan = src.plan(None, &opts)?;
            let db = Db::create_in(&path, plan.schema.clone(), db_options(directory, threads), &models)?;
            let e = embed_errors_of(&db, &models);
            let r = src.load(&db, &plan)?;
            (db, e, Some(r.to_json().to_string()))
        }
        other => return Err(bad(format!("no way to open a database called {other:?}"))),
    };
    // Segments merge in the background as writes add them, one merge at a
    // time, as a server's do (as strata-py's).
    db.spawn_merger(MergerOptions { threads: 1, ..MergerOptions::default() });
    Ok(Opened { db: with_threads(db, threads), embed_errors, last_import })
}

// ---------------------------------------------------------------- files

/// A file to read: the path given, or what an `s3://` URL names, read into
/// a temporary directory removed with this (feature `s3`).
struct Local {
    path: PathBuf,
    #[cfg(feature = "s3")]
    _held: Option<strata::objects::Input>,
}

fn local(path: &Path) -> Result<Local> {
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

fn remote(path: &Path) -> Result<Remote> {
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
    fn finish(self) -> Result<()> {
        #[cfg(feature = "s3")]
        if let Some(o) = self.out {
            o.finish()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- operations

/// The schema of the table named `table` (any case), or of the first.
fn table_of(db: &Db, table: Option<&str>) -> Result<Arc<Schema>> {
    match table {
        None => Ok(db.catalog().swap_remove(0)),
        Some(t) => db.storage().table(t).ok_or_else(|| bad(format!("no such table: {t}"))),
    }
}

/// One change of an `alter` ([`Db::alter_table`]).
fn alter_of(j: &Json) -> Result<strata::schema::Alter> {
    use strata::schema::{Alter, Field};
    let wrong = || bad(format!("a change is {{\"add\": field}}, {{\"drop\": name}} or {{\"rename\": [from, to]}}, not {j}"));
    let o = j.as_object().filter(|o| o.len() == 1).ok_or_else(wrong)?;
    match o.iter().next().ok_or_else(wrong)? {
        (k, f) if k == "add" => Ok(Alter::AddField(Field::from_short_json(f)?)),
        (k, Json::String(name)) if k == "drop" => Ok(Alter::DropField(name.clone())),
        (k, Json::Array(xs)) if k == "rename" => match xs.as_slice() {
            [Json::String(from), Json::String(to)] => Ok(Alter::RenameField { from: from.clone(), to: to.clone() }),
            _ => Err(wrong()),
        },
        _ => Err(wrong()),
    }
}

/// A row to write: its values as JSON, and the vectors that came apart
/// from it as float32, by field.
struct Row {
    json: Json,
    vectors: Vec<(usize, Vec<f32>)>,
}

/// The rows of an insert or upsert: `rows`, and `vectors` placed by the
/// argument "vectors", a list of `[row, field]` as long as `vectors`.
/// A field that does not take one vector a row gets its array as JSON.
fn rows_of(a: &mut Args, schema: &Schema, vectors: Vec<Vec<f32>>) -> Result<Vec<Row>> {
    let places = a.list("vectors");
    let mut rows: Vec<Row> = a.list("rows").into_iter().map(|json| Row { json, vectors: Vec::new() }).collect();
    if places.len() != vectors.len() {
        return Err(bad(format!("{} vectors given for {} places", vectors.len(), places.len())));
    }
    for (place, xs) in places.iter().zip(vectors) {
        let (i, name) = match place.as_array().map(Vec::as_slice) {
            Some([Json::Number(i), Json::String(name)]) => (i.as_u64().unwrap_or(u64::MAX) as usize, name.as_str()),
            _ => return Err(bad(format!("a vector's place is [row, field], not {place}"))),
        };
        let row = rows.get_mut(i).ok_or_else(|| bad(format!("a vector for row {i} of {}", place)))?;
        match schema.field_index(name).filter(|&f| ingest::takes_vector(&schema.fields[f])) {
            Some(f) => row.vectors.push((f, xs)),
            None => {
                let Json::Object(m) = &mut row.json else { return Err(bad(format!("row {} is not an object", i + 1))) };
                m.insert(name.to_string(), Json::Array(xs.into_iter().map(|x| json!(x)).collect()));
            }
        }
    }
    Ok(rows)
}

/// Documents of `rows`, the first numbered `first` in errors.
fn documents(db: &Db, schema: &Schema, rows: Vec<Row>, first: usize) -> Result<Vec<strata::Document>> {
    let now = strata::timestamp::now();
    let mut docs = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| ingest::document_with_vectors(schema, &r.json, r.vectors, now).map_err(|e| Error::Schema(format!("row {}: {e}", first + i))))
        .collect::<Result<Vec<_>>>()?;
    db.fill_in(&schema.table, &mut docs)?;
    Ok(docs)
}

fn insert(db: &Db, a: &mut Args, vectors: Vec<Vec<f32>>) -> Result<Json> {
    let schema = table_of(db, a.str("table")?)?;
    let rows = rows_of(a, &schema, vectors)?;
    let docs = documents(db, &schema, rows, 1)?;
    let n = docs.len();
    if n >= BULK_INSERT {
        db.load_into(&schema.table, docs)?;
    } else {
        // Threads inserting at once share commits.
        db.write_ops(docs.into_iter().map(|doc| WriteOp::Insert { table: schema.table.clone(), doc }).collect())?;
    }
    Ok(json!(n))
}

fn upsert(db: &Db, a: &mut Args, vectors: Vec<Vec<f32>>) -> Result<Json> {
    let schema = table_of(db, a.str("table")?)?;
    let rows = rows_of(a, &schema, vectors)?;
    let mut n = 0;
    let mut rows = rows.into_iter().peekable();
    while rows.peek().is_some() {
        let chunk: Vec<Row> = rows.by_ref().take(BATCH).collect();
        let len = chunk.len();
        let docs = documents(db, &schema, chunk, n + 1)?;
        db.write_ops(docs.into_iter().map(|doc| WriteOp::Upsert { table: schema.table.clone(), doc }).collect())?;
        n += len;
    }
    Ok(json!(n))
}

fn delete(db: &Db, a: &mut Args) -> Result<Json> {
    let s = table_of(db, a.str("table")?)?;
    let keys = a.list("keys");
    let pk = s.primary_key.ok_or_else(|| bad("deleting by key needs a primary key"))?;
    let ops = keys.iter().map(|k| Ok(WriteOp::Delete { table: s.table.clone(), key: ingest::value(&s, pk, k)? })).collect::<Result<Vec<_>>>()?;
    Ok(json!(db.write_ops(ops)?))
}

/// The rows with these keys, read by key without SQL ([`Db::get_many`]):
/// per key, in their order, an object of the columns `SELECT *` answers,
/// or null.
fn get_many(db: &Db, a: &mut Args) -> Result<Json> {
    let s = table_of(db, a.str("table")?)?;
    let pk = s.primary_key.ok_or_else(|| bad(format!("{} has no primary key: rows are read by key only from a table with one", s.table)))?;
    let keys = a.list("keys").iter().map(|k| ingest::value(&s, pk, k)).collect::<Result<Vec<_>>>()?;
    let columns = Db::get_columns(&s);
    let rows = db.get_many(&s.table, &keys)?;
    Ok(Json::Array(
        rows.iter()
            .map(|r| match r {
                None => Json::Null,
                Some(row) => Json::Object(columns.iter().zip(row).map(|(c, v)| (c.clone(), to_json(v))).collect()),
            })
            .collect(),
    ))
}

/// Write operations as strata-server's /v1/write takes them, in one
/// transaction: insert, upsert and delete, and set and add of a numeric
/// field in place (an add with a "doc" inserts it when no row has its
/// key). Returns the rows written.
fn write(db: &Db, a: &mut Args) -> Result<Json> {
    let ops = a.list("ops");
    let wrong = |i: usize, m: &str| bad(format!("operation {}: {m}", i + 1));
    // Read before the write starts: threads writing at once share its
    // commit.
    let mut write = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        let get = |k: &str| op.get(k).filter(|v| !v.is_null());
        let kind = get("op").and_then(Json::as_str).ok_or_else(|| wrong(i, "it needs \"op\""))?;
        let s = table_of(db, get("table").and_then(Json::as_str))?;
        let table = s.table.clone();
        let doc = || match get("doc") {
            Some(d) => Ok(documents(db, &s, vec![Row { json: d.clone(), vectors: Vec::new() }], 1)?.remove(0)),
            None => Err(wrong(i, &format!("{kind} needs \"doc\""))),
        };
        let key = || {
            let pk = s.primary_key.ok_or_else(|| wrong(i, "writing by key needs a primary key"))?;
            ingest::value(&s, pk, get("key").ok_or_else(|| wrong(i, &format!("{kind} needs \"key\"")))?)
        };
        let field = || get("field").and_then(Json::as_str).map(str::to_string).ok_or_else(|| wrong(i, &format!("{kind} needs \"field\"")));
        write.push(match kind {
            "insert" => WriteOp::Insert { table, doc: doc()? },
            "upsert" => WriteOp::Upsert { table, doc: doc()? },
            "delete" => WriteOp::Delete { table, key: key()? },
            "set" => {
                let f = field()?;
                let fi = s.field_index(&f).ok_or_else(|| wrong(i, &format!("no field {f}")))?;
                WriteOp::Set { table, key: key()?, value: ingest::value(&s, fi, get("value").unwrap_or(&Json::Null))?, field: f }
            }
            "add" => {
                let delta = match get("value") {
                    Some(Json::Number(x)) => x.as_i64().map(Value::Int).or_else(|| x.as_f64().map(Value::Float)),
                    _ => None,
                };
                let delta = delta.ok_or_else(|| wrong(i, "add needs a numeric value"))?;
                if get("doc").is_some() {
                    let d = doc()?;
                    if get("key").is_some() && Some(&key()?) != s.primary_key.map(|pk| &d[pk]) {
                        return Err(wrong(i, "add's key and its doc's key differ: give one"));
                    }
                    WriteOp::AddOrInsert { table, doc: d, field: field()?, delta }
                } else {
                    WriteOp::Add { table, key: key()?, field: field()?, delta }
                }
            }
            other => return Err(wrong(i, &format!("unknown op {other:?}"))),
        });
    }
    Ok(json!(db.write_ops(write)?))
}

/// The vector of a text by an embedded field's model, or of an image or a
/// sound (base64) by a model of that.
fn embed(db: &Db, a: &Args) -> Result<Json> {
    let table = a.str("table")?;
    let file = match (a.str("image")?, a.str("audio")?) {
        (Some(_), Some(_)) => return Err(bad("embed takes a text, an image or a sound, one of them")),
        (Some(d), None) => Some(("image", d)),
        (None, Some(d)) => Some(("audio", d)),
        (None, None) => None,
    };
    if let Some((kind, b64)) = file {
        let s = table_of(db, table)?;
        let (m, data) = strata::embed::query_file(kind, b64)?;
        let what = strata::embed::modality_name(m);
        let field = match a.str("field")? {
            Some(f) => f.to_string(),
            None => s.fields.iter().find(|f| db.embedder_in(&s.table, &f.name).is_some_and(|e| e.modalities().contains(&m))).map(|f| f.name.clone()).ok_or_else(|| bad(format!("no embedded field has a model of {what} open")))?,
        };
        let e = db.embedder_in(&s.table, &field).ok_or_else(|| bad(format!("{field} has no embedding model open (see open's models)")))?;
        return Ok(json!(strata::embed::embed_file(e.as_ref(), &data)?));
    }
    let text = a.need_str("text")?;
    let mut o = Map::new();
    o.insert("embed".into(), json!(text));
    if let Some(f) = a.str("field")? {
        o.insert("field".into(), json!(f));
    }
    if let Some(t) = table {
        o.insert("table".into(), json!(t));
    }
    match param(db, &Json::Object(o))? {
        Value::Vector(v) => Ok(json!(v.to_vec())),
        _ => Err(bad("the field's model embeds a text a vector a token (a multi-vector field): embedTokens gives them")),
    }
}

fn train_options(a: &Args) -> Result<TrainOptions> {
    let d = TrainOptions::default();
    Ok(TrainOptions {
        levels: a.uint("levels")?.map_or(d.levels, |n| n as usize),
        k: a.uint("k")?.map_or(d.k, |n| n as usize),
        iters: a.uint("iterations")?.map_or(d.iters, |n| n as usize),
        sample: a.uint("sample")?.map_or(d.sample, |n| n as usize),
        seed: a.uint("seed")?.unwrap_or(d.seed),
    })
}

/// Runs operation `op` of a database with its arguments.
fn dispatch(db: &Db, op: &str, mut a: Args, vectors: Vec<Vec<f32>>) -> Result<Json> {
    if !vectors.is_empty() && !matches!(op, "insert" | "upsert") {
        return Err(bad(format!("{op} takes no vectors apart from its arguments")));
    }
    let a = &mut a;
    match op {
        "schema" => Ok(table_of(db, a.str("table")?)?.to_short_json()),
        "tables" => Ok(Json::Array(db.catalog().iter().map(|s| s.to_short_json()).collect())),
        "count" => Ok(json!(db.snapshot().n_live_docs())),
        "alter" => {
            let table = table_of(db, a.str("table")?)?.table.clone();
            let alters = a.list("alters").iter().map(alter_of).collect::<Result<Vec<_>>>()?;
            db.alter_table(&table, &alters)?;
            Ok(Json::Null)
        }
        "query" => {
            let p = params(db, a.get("params"))?;
            Ok(result_json(&db.query(a.need_str("sql")?, &p)?))
        }
        "sql" => {
            let p = params(db, a.get("params"))?;
            Ok(sql_json(db.sql(a.need_str("sql")?, &p)?))
        }
        "query_logged" => {
            let p = params(db, a.get("params"))?;
            let log = Log::from_json(a.get("log").ok_or_else(|| bad("log is missing"))?)?;
            let l = db.query_logged(a.need_str("sql")?, &p, &log)?;
            let mut out = result_json(&l.rs);
            out["request"] = json!(l.request);
            if !l.teams.is_empty() {
                out["teams"] = json!(l.teams);
            }
            Ok(out)
        }
        "export_rank_log" => {
            let x = Export::from_json(a.get("export").ok_or_else(|| bad("export is missing"))?)?;
            let mut out = Vec::new();
            db.export_rank_log(&x, |r| {
                out.push(r);
                Ok(())
            })?;
            Ok(Json::Array(out))
        }
        "plan" => {
            let p = params(db, a.get("params"))?;
            let text = db.explain(a.need_str("sql")?, &p)?.to_json_text()?;
            serde_json::from_str(&text).map_err(|e| bad(format!("the plan: {e}")))
        }
        "run" => {
            let plan = Plan::from_json(a.get("plan").ok_or_else(|| bad("plan is missing"))?)?;
            Ok(sql_json(db.run(&plan)?))
        }
        "insert" => insert(db, a, vectors),
        "upsert" => upsert(db, a, vectors),
        "delete" => delete(db, a),
        "get_many" => get_many(db, a),
        "expire" => Ok(json!(db.expire(strata::timestamp::now(), 10_000)?)),
        "write" => write(db, a),
        "import_file" => {
            let format = match a.str("format")? {
                None => None,
                Some(f) => Some(Format::parse(f).ok_or_else(|| bad(format!("unknown format {f:?}: jsonl, json, csv or tsv")))?),
            };
            let l = local(&a.need_path("path")?)?;
            let s = table_of(db, a.str("table")?)?;
            let r = ingest::import_file_into(db, &s.table, &l.path, format)?;
            Ok(json!({"rows": r.rows, "bulk": r.bulk, "skipped_fields": r.unknown}))
        }
        "import_sqlite" => {
            let opts = sqlite_options(a)?;
            let l = local(&a.need_path("path")?)?;
            Ok(sqlite::import(db, &l.path, &opts)?.to_json())
        }
        "export_sqlite" => {
            let opts = sqlite::ExportOptions { tables: a.strings("tables")?.unwrap_or_default(), fts: a.bool("fts", true)?, indexes: a.bool("indexes", true)?, meta: a.bool("schemas", true)? };
            let out = remote(&a.need_path("path")?)?;
            let r = sqlite::export(db, &out.path, &opts)?;
            out.finish()?;
            Ok(r.to_json())
        }
        #[cfg(feature = "parquet")]
        "import_parquet" => {
            let opts = parquet_options(a)?;
            let l = local(&a.need_path("path")?)?;
            Ok(strata::parquet::import(db, &l.path, &opts)?.to_json())
        }
        #[cfg(feature = "parquet")]
        "export_parquet" => {
            let opts = strata::parquet::ExportOptions { tables: a.strings("tables")?.unwrap_or_default(), meta: a.bool("schemas", true)? };
            let out = remote(&a.need_path("path")?)?;
            let r = strata::parquet::export(db, &out.path, &opts)?;
            out.finish()?;
            Ok(r.to_json())
        }
        #[cfg(feature = "parquet")]
        "plan_parquet" => {
            let opts = parquet_options(a)?;
            let l = local(&a.need_path("parquet_path")?)?;
            Ok(strata::parquet::plan(&l.path, Some(db), &opts)?.to_json())
        }
        "embed" => embed(db, a),
        "embed_tokens" => {
            let table = table_of(db, a.str("table")?)?.table.clone();
            let field = a.need_str("field")?;
            match db.embed_query_in(&table, field, a.need_str("text")?)? {
                Value::List(vs) => Ok(Json::Array(vs.iter().filter_map(|v| if let Value::Vector(v) = v { Some(json!(v.to_vec())) } else { None }).collect())),
                _ => Err(bad(format!("{field} is a field of one vector: embed gives it"))),
            }
        }
        "sid_next" => {
            let p = params(db, a.get("params"))?;
            let table = table_of(db, a.str("table")?)?.table.clone();
            let prefixes = a.strings("prefixes")?.unwrap_or_default();
            let got = db.sid_next(&table, a.need_str("field")?, &prefixes, a.str("where")?, &p)?;
            Ok(Json::Object(prefixes.into_iter().zip(got).map(|(p, cs)| (p, Json::Array(cs.into_iter().map(|(c, n)| json!([c, n])).collect()))).collect()))
        }
        "train_codebook" => {
            let table = match a.str("table")? {
                Some(t) => t.to_string(),
                None => db.schema().table.clone(),
            };
            let models = models_of(a)?;
            Ok(json!(db.train_codebook(&table, a.need_str("field")?, a.need_str("name")?, &models, &train_options(a)?)?))
        }
        // As strata-py's: the memtable written out, segments merged into
        // as few as can be, the space freed returned to the file system.
        "optimize" => {
            db.optimize()?;
            Ok(Json::Null)
        }
        "vacuum" => Ok(Json::from(db.storage().vacuum()?)),
        other => Err(bad(format!("no operation {other:?}"))),
    }
}

/// Operations of no database: "plan_sqlite" ({sqlite_path, tables?,
/// schemas?, blobs_base64?}), "plan_parquet" ({parquet_path, table?,
/// schema?, key?, blobs_base64?}), "upgrade" ({path}) and
/// "train_codebook" ({vectors, name, models?, levels?, k?, iterations?,
/// sample?, seed?}).
fn dispatch_static(op: &str, a: Args) -> Result<Json> {
    match op {
        "plan_sqlite" => {
            let opts = sqlite_options(&a)?;
            let l = local(&a.need_path("sqlite_path")?)?;
            Ok(sqlite::plan(&l.path, None, &opts)?.to_json())
        }
        #[cfg(feature = "parquet")]
        "plan_parquet" => {
            let opts = parquet_options(&a)?;
            let l = local(&a.need_path("parquet_path")?)?;
            Ok(strata::parquet::plan(&l.path, None, &opts)?.to_json())
        }
        "upgrade" => Ok(match strata::upgrade::upgrade(&a.need_path("path")?)? {
            None => Json::Null,
            Some(u) => json!({"old": u.old.to_string_lossy(), "tables": u.tables, "rows": u.rows}),
        }),
        "train_codebook" => {
            let rows = match a.get("vectors") {
                Some(Json::Array(rows)) => rows,
                _ => return Err(bad("vectors are a list of lists of numbers")),
            };
            let dim = match rows.first() {
                Some(Json::Array(v)) => v.len(),
                _ => return Err(bad("vectors are a list of lists of numbers, at least one")),
            };
            let mut flat = Vec::with_capacity(rows.len() * dim);
            for (i, r) in rows.iter().enumerate() {
                match r {
                    Json::Array(v) if v.len() == dim => {
                        for x in v {
                            flat.push(x.as_f64().ok_or_else(|| bad(format!("vector {}: {x} is not a number", i + 1)))? as f32);
                        }
                    }
                    _ => return Err(bad(format!("vector {} does not have {dim} numbers, as the first does", i + 1))),
                }
            }
            let models = models_of(&a)?;
            Ok(json!(Codebook::train(&flat, dim, &train_options(&a)?)?.save(a.need_str("name")?, &models)?))
        }
        other => Err(bad(format!("no operation {other:?}"))),
    }
}

/// Runs `f`, a panic as an error: a bug in Strata fails the call that met
/// it, not the process.
fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let what = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
        Err(Error::Query(format!("strata panicked: {what}")))
    })
}

// ---------------------------------------------------------------- tasks

type Work = Box<dyn FnOnce() -> Result<Json> + Send>;

/// A call run on libuv's thread pool, whose answer is JSON text.
pub struct Job(Option<Work>);

impl Task for Job {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> napi::Result<String> {
        let f = self.0.take().ok_or_else(|| napi::Error::from_reason("a call ran twice"))?;
        guarded(f).map(|j| j.to_string()).map_err(napi_err)
    }

    fn resolve(&mut self, _: Env, out: String) -> napi::Result<String> {
        Ok(out)
    }
}

fn job(f: impl FnOnce() -> Result<Json> + Send + 'static) -> AsyncTask<Job> {
    AsyncTask::new(Job(Some(Box::new(f))))
}

/// A database's opening, run on libuv's thread pool.
pub struct OpenJob {
    op: String,
    args: String,
}

impl Task for OpenJob {
    type Output = Opened;
    type JsValue = NativeDatabase;

    fn compute(&mut self) -> napi::Result<Opened> {
        guarded(|| open_db(&self.op, &Args::parse(&self.args)?)).map_err(napi_err)
    }

    fn resolve(&mut self, _: Env, o: Opened) -> napi::Result<NativeDatabase> {
        Ok(NativeDatabase { db: Arc::new(RwLock::new(Some(Arc::new(o.db)))), embed_errors: o.embed_errors, last_import: o.last_import })
    }
}

fn copied(vectors: Option<Vec<Float32Array>>) -> Vec<Vec<f32>> {
    vectors.unwrap_or_default().iter().map(|v| v.to_vec()).collect()
}

// ---------------------------------------------------------------- the class

/// A database file (or directory) opened in this process. Only one process
/// may have a database open at a time.
#[napi]
pub struct NativeDatabase {
    db: Arc<RwLock<Option<Arc<Db>>>>,
    embed_errors: Vec<(String, String)>,
    last_import: Option<String>,
}

impl NativeDatabase {
    fn get(&self) -> napi::Result<Arc<Db>> {
        self.db.read().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| napi::Error::from_reason("the database is closed"))
    }
}

#[napi]
impl NativeDatabase {
    /// Opens or creates a database ([`open_db`]): `op` is "create",
    /// "open", "from_sqlite" or "from_parquet", `args` its arguments as
    /// JSON text.
    #[napi]
    pub fn open(op: String, args: String) -> AsyncTask<OpenJob> {
        AsyncTask::new(OpenJob { op, args })
    }

    /// Runs operation `op` with `args` (JSON text) on the thread pool;
    /// answers its result as JSON text. `vectors` are the vectors of an
    /// insert's or upsert's rows, placed by its argument "vectors".
    #[napi(ts_return_type = "Promise<string>")]
    pub fn call(&self, op: String, args: String, vectors: Option<Vec<Float32Array>>) -> napi::Result<AsyncTask<Job>> {
        let vectors = copied(vectors);
        if op == "close" {
            let held = self.db.clone();
            return Ok(job(move || {
                let db = held.write().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(db) = db {
                    // Small segments merged, and the space merges freed
                    // returned to the file system when it is an eighth of
                    // the file or more.
                    db.close()?;
                }
                Ok(Json::Null)
            }));
        }
        let db = self.get()?;
        Ok(job(move || dispatch(&db, &op, Args::parse(&args)?, vectors)))
    }

    /// [`NativeDatabase::call`] on the calling thread.
    #[napi]
    pub fn call_sync(&self, op: String, args: String, vectors: Option<Vec<Float32Array>>) -> napi::Result<String> {
        let db = self.get()?;
        let vectors = copied(vectors);
        guarded(|| dispatch(&db, &op, Args::parse(&args)?, vectors)).map(|j| j.to_string()).map_err(napi_err)
    }

    /// Embedded fields whose model did not open, with the reason, as JSON
    /// text of an object.
    #[napi(getter)]
    pub fn embed_errors(&self) -> String {
        Json::Object(self.embed_errors.iter().map(|(f, e)| (f.clone(), json!(e))).collect()).to_string()
    }

    /// What the import that made this database did, as JSON text
    /// (`from_sqlite`, `from_parquet`).
    #[napi(getter)]
    pub fn last_import(&self) -> Option<String> {
        self.last_import.clone()
    }

    #[napi(getter)]
    pub fn closed(&self) -> bool {
        self.db.read().unwrap_or_else(|e| e.into_inner()).is_none()
    }
}

/// Runs an operation of no database ([`dispatch_static`]) on the thread
/// pool; answers its result as JSON text.
#[napi(ts_return_type = "Promise<string>")]
pub fn call_static(op: String, args: String) -> AsyncTask<Job> {
    job(move || dispatch_static(&op, Args::parse(&args)?))
}

/// This addon's version.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
