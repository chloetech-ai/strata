//! Python bindings for Strata, the database in-process: the `strata._strata`
//! extension module under the `strata` package (`python/strata`), which
//! wraps it in the documented API.
//!
//! Rows cross the boundary as JSON-like Python values (dicts, lists,
//! strings, numbers, None), read by the same code as files and the HTTP
//! API ([`strata::ingest`]); a row's vector given as a numpy array is read
//! from the array's bytes instead. Every call that reads or writes the
//! database releases the GIL.

#[cfg(feature = "datafusion")]
mod arrow;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::intern;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::{Map, Number, Value as Json};
use strata::db::{Db, SqlResult, WriteOp};
use strata::ingest::{self, Format};
use strata::query::ir::Plan;
use strata::query::sql::{ident, quoted};
use strata::query::Params;
use strata::query::exec::ResultSet;
use strata::rank_log::{Export, Log};
use strata::schema::Schema;
use strata::sid::{Codebook, TrainOptions};
use strata::sqlite;
use strata::segment::build::BuildOptions;
use strata::storage::{DbOptions, Layout, MergerOptions};
use strata::{FieldType, Value};

create_exception!(_strata, StrataError, PyException, "An error from the database: a bad schema, query, value or file.");

fn err(e: strata::Error) -> PyErr {
    StrataError::new_err(e.to_string())
}

/// Rows a write transaction carries (as `strata-server import` does).
const BATCH: usize = ingest::IMPORT_BATCH;
/// Inserts of at least this many rows, the memtable's size, are bulk-loaded
/// into new segments in one commit. Fewer go through the write-ahead log
/// into the memtable, which gathers the rows of many inserts into one
/// segment when it fills (the insert that fills it builds it): an app
/// inserting 2,000 rows at a time makes one segment of 10,000 rows where it
/// made five, and the merges after them have a fifth of the segments to
/// join.
const BULK_INSERT: usize = 10_000;

/// A Python value as JSON. Numpy arrays and scalars (anything with
/// `tolist()`) become lists and numbers.
fn to_json(o: &Bound<'_, PyAny>) -> PyResult<Json> {
    if o.is_none() {
        return Ok(Json::Null);
    }
    if let Ok(b) = o.cast::<PyBool>() {
        return Ok(Json::Bool(b.is_true()));
    }
    if let Ok(i) = o.cast::<PyInt>() {
        return Ok(Json::Number(i.extract::<i64>()?.into()));
    }
    if let Ok(f) = o.cast::<PyFloat>() {
        return Ok(Number::from_f64(f.value()).map_or(Json::Null, Json::Number));
    }
    if let Ok(s) = o.cast::<PyString>() {
        return Ok(Json::String(s.to_str()?.to_string()));
    }
    if let Ok(d) = o.cast::<PyDict>() {
        let mut m = Map::new();
        for (k, v) in d.iter() {
            let k = k.cast::<PyString>().map_err(|_| PyValueError::new_err(format!("keys are strings, not {}", k.repr().map(|r| r.to_string()).unwrap_or_default())))?;
            m.insert(k.to_str()?.to_string(), to_json(&v)?);
        }
        return Ok(Json::Object(m));
    }
    if let Ok(l) = o.cast::<PyList>() {
        return l.iter().map(|x| to_json(&x)).collect::<PyResult<_>>().map(Json::Array);
    }
    if let Ok(t) = o.cast::<PyTuple>() {
        return t.iter().map(|x| to_json(&x)).collect::<PyResult<_>>().map(Json::Array);
    }
    if o.hasattr("tolist")? {
        return to_json(&o.call_method0("tolist")?);
    }
    // A datetime or date, as ISO 8601 text (a TIMESTAMP's form; a naive
    // datetime is taken as UTC).
    if o.hasattr("isoformat")? && o.hasattr("year")? {
        return Ok(Json::String(o.call_method0("isoformat")?.extract()?));
    }
    Err(PyValueError::new_err(format!("cannot store a {}: values are None, bool, int, float, str, datetime, lists and dicts", o.get_type().name()?)))
}

/// A stored value as Python.
fn to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        Value::Null => py.None().into_bound(py),
        Value::Int(i) => i.into_pyobject(py)?.into_any(),
        Value::Float(f) => f.into_pyobject(py)?.into_any(),
        Value::Str(s) => s.into_pyobject(py)?.into_any(),
        Value::Vector(xs) => PyList::new(py, xs.iter().map(|&x| x as f64))?.into_any(),
        Value::List(xs) => PyList::new(py, xs.iter().map(|x| to_py(py, x)).collect::<PyResult<Vec<_>>>()?)?.into_any(),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any(),
        // As the HTTP API gives it, so that Database and Client agree.
        Value::Timestamp(t) => strata::timestamp::format(*t).into_pyobject(py)?.into_any(),
    })
}

/// A query's answer as a dict: columns, rows (lists), keys, scores, stats.
fn result_py<'py>(py: Python<'py>, rs: &ResultSet) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("columns", PyList::new(py, &rs.columns)?)?;
    let rows = rs.rows.iter().map(|r| PyList::new(py, r.iter().map(|v| to_py(py, v)).collect::<PyResult<Vec<_>>>()?)).collect::<PyResult<Vec<_>>>()?;
    d.set_item("rows", PyList::new(py, rows)?)?;
    d.set_item("keys", PyList::new(py, rs.keys.iter().map(|v| to_py(py, v)).collect::<PyResult<Vec<_>>>()?)?)?;
    d.set_item("scores", PyList::new(py, &rs.scores)?)?;
    d.set_item("stats", serde_json::to_string(&rs.stats).unwrap_or_default())?;
    Ok(d)
}

/// A query parameter, as the HTTP API reads one: a number, string, None,
/// a list of numbers (a vector), a list, a dict of term weights (SPARSE),
/// `{"embed": text, "field": f}` (the text's vector by `f`'s model, or an
/// image or sound file's for `{"embed": {"image": "<base64>"}}` or
/// `{"embed": {"audio": "<base64>"}}`) or
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
            let e = db.embedder_in(&s.table, &field).ok_or_else(|| bad(format!("{field} has no embedding model open (see Database.open's models)")))?;
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
        Json::Object(_) => return Err(bad("parameters are numbers, strings, None, lists, dicts of term weights, {\"embed\": text} or {\"vector_of\": key}".into())),
    })
}

fn params(db: &Db, j: &[(String, Json)]) -> strata::Result<Params> {
    let mut p = Params::new();
    for (k, v) in j {
        p = p.set(k, param(db, v)?);
    }
    Ok(p)
}

fn params_json(p: Option<&Bound<'_, PyDict>>) -> PyResult<Vec<(String, Json)>> {
    let Some(p) = p else { return Ok(Vec::new()) };
    p.iter().map(|(k, v)| Ok((k.extract::<String>()?, to_json(&v)?))).collect()
}

fn rows_json(rows: &Bound<'_, PyAny>) -> PyResult<Vec<Json>> {
    if rows.cast::<PyDict>().is_ok() {
        return Ok(vec![to_json(rows)?]);
    }
    rows.try_iter()?.map(|r| to_json(&r?)).collect()
}

/// A row to write: its values as JSON, except the vectors of fields that
/// take one ([`ingest::takes_vector`]) given as arrays ([`array_f32`]),
/// which stay float32.
struct Row {
    json: Json,
    vectors: Vec<(usize, Vec<f32>)>,
}

/// The rows of `rows` (a dict, or an iterable of them) for `schema`.
fn rows_of(rows: &Bound<'_, PyAny>, schema: &Schema) -> PyResult<Vec<Row>> {
    if rows.cast::<PyDict>().is_ok() {
        return Ok(vec![row_of(rows, schema)?]);
    }
    rows.try_iter()?.map(|r| row_of(&r?, schema)).collect()
}

fn row_of(o: &Bound<'_, PyAny>, schema: &Schema) -> PyResult<Row> {
    let Ok(d) = o.cast::<PyDict>() else { return Ok(Row { json: to_json(o)?, vectors: Vec::new() }) };
    let mut m = Map::new();
    let mut vectors = Vec::new();
    for (k, v) in d.iter() {
        let k = k.cast::<PyString>().map_err(|_| PyValueError::new_err(format!("keys are strings, not {}", k.repr().map(|r| r.to_string()).unwrap_or_default())))?;
        let k = k.to_str()?;
        if let Some(f) = schema.field_index(k).filter(|&f| ingest::takes_vector(&schema.fields[f])) {
            if let Some(xs) = array_f32(&v)? {
                vectors.push((f, xs));
                continue;
            }
        }
        m.insert(k.to_string(), to_json(&v)?);
    }
    Ok(Row { json: Json::Object(m), vectors })
}

/// `o`'s numbers if it is a one-dimensional array of float32 or float64 in
/// this machine's byte order, by the array interface (numpy's): read from
/// its bytes, without the Python float per number that `tolist()` makes
/// and JSON's number after it. `None` for anything else.
fn array_f32(o: &Bound<'_, PyAny>) -> PyResult<Option<Vec<f32>>> {
    if o.cast::<PyList>().is_ok() || o.cast::<PyTuple>().is_ok() {
        return Ok(None);
    }
    let py = o.py();
    let Ok(ai) = o.getattr(intern!(py, "__array_interface__")) else { return Ok(None) };
    let Ok(ai) = ai.cast::<PyDict>() else { return Ok(None) };
    let (Some(ty), Some(shape)) = (ai.get_item("typestr")?, ai.get_item("shape")?) else { return Ok(None) };
    let ty: String = ty.extract()?;
    let native = if cfg!(target_endian = "little") { "<" } else { ">" };
    let width = match ty.strip_prefix(native).or_else(|| ty.strip_prefix('=')) {
        Some("f4") => 4,
        Some("f8") => 8,
        _ => return Ok(None),
    };
    if shape.len()? != 1 {
        return Ok(None);
    }
    let bytes = o.call_method0(intern!(py, "tobytes"))?;
    let b = bytes.cast::<PyBytes>()?.as_bytes();
    Ok(Some(match width {
        4 => b.chunks_exact(4).map(|c| f32::from_ne_bytes(c.try_into().expect("four bytes"))).collect(),
        _ => b.chunks_exact(8).map(|c| f64::from_ne_bytes(c.try_into().expect("eight bytes")) as f32).collect(),
    }))
}

/// The cores a database uses unless `threads` says otherwise: the machine's
/// (on Linux, its cgroup's quota).
fn threads_or_cores(threads: Option<usize>) -> PyResult<usize> {
    match threads {
        Some(0) => Err(PyValueError::new_err("threads: at least 1")),
        Some(n) => Ok(n),
        None => Ok(std::thread::available_parallelism().map_or(1, |n| n.get())),
    }
}

/// How this module opens a database, as strata-server does: segment builds
/// (flushes, merges) build vector indexes on `threads` cores, and the newest
/// in-memory chunk keeps its vectors flat, since a graph over so few rows
/// costs more to build at every write than an exact scan of them costs a
/// query.
///
/// Each write publishes its snapshot while queries come in
/// (`DbOptions::publish_snapshots`): the insert that fills a memtable chunk
/// indexes it before it returns. Without that the first query after it
/// did, while every other query waited for it, and a stream of inserts
/// beside queries left the queries waiting on chunk after chunk (on 4
/// cores, inserts of 2,500 rows 20 times a second beside 4 threads
/// querying: 77 queries a second with a p99 of 1.2 s, against 5,200 with
/// a p99 of 6 ms).
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

/// Queries search segments on up to `threads` cores: the calling thread and
/// the process's shared pool, as its threads come free.
fn with_threads(mut db: Db, threads: usize) -> Db {
    db.exec.threads = threads;
    db
}

/// A database file (or directory) opened in this process. Only one process
/// may have a database open at a time.
#[pyclass(frozen, module = "strata._strata")]
struct Database {
    db: RwLock<Option<Arc<Db>>>,
    /// Embedded fields whose model did not open, with the reason.
    #[pyo3(get)]
    embed_errors: Vec<(String, String)>,
}

impl Database {
    fn new(db: Db, models: Option<PathBuf>) -> Self {
        let models = models.unwrap_or_else(strata::embed::models_dir);
        let embed_errors = db.open_embedders(&models).into_iter().map(|(f, e)| (f, e.to_string())).collect();
        Database::of(db, embed_errors)
    }

    /// The database `db`, merging its segments in the background as writes
    /// add them (one merge at a time), as a server does: without it a
    /// table that is written to for a long time collects segments, each a
    /// query visits, and the rows its updates and deletes replaced.
    fn of(db: Db, embed_errors: Vec<(String, String)>) -> Self {
        db.spawn_merger(MergerOptions { threads: 1, ..MergerOptions::default() });
        Database { db: RwLock::new(Some(Arc::new(db))), embed_errors }
    }

    fn get(&self) -> PyResult<Arc<Db>> {
        self.db.read().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| StrataError::new_err("the database is closed"))
    }

    fn sqlite_options(tables: Option<Vec<String>>, schemas: Option<&str>, blobs_base64: bool) -> PyResult<sqlite::ImportOptions> {
        let schemas = schemas.map(sqlite::schemas_from_json).transpose().map_err(err)?.unwrap_or_default();
        Ok(sqlite::ImportOptions { tables: tables.unwrap_or_default(), schemas, blobs_base64 })
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
}

/// A file to read: the path given, or what an `s3://` URL names, read into
/// a temporary directory removed with this (feature `s3`).
struct Local {
    path: PathBuf,
    #[cfg(feature = "s3")]
    _held: Option<strata::objects::Input>,
}

fn local(path: &std::path::Path) -> strata::Result<Local> {
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

fn remote(path: &std::path::Path) -> strata::Result<Remote> {
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

#[cfg(feature = "parquet")]
fn parquet_options(table: Option<String>, schema: Option<&str>, key: Option<String>, blobs_base64: bool) -> PyResult<strata::parquet::ImportOptions> {
    let schema = schema.map(Schema::from_json).transpose().map_err(err)?;
    Ok(strata::parquet::ImportOptions { table, schema, key, blobs_base64 })
}

/// The schema of the table named `table` (any case), or of the first.
/// One change of [`Database::alter_table_json`].
fn alter_of(j: &serde_json::Value) -> PyResult<strata::schema::Alter> {
    use strata::schema::{Alter, Field};
    let bad = || PyValueError::new_err(format!("a change is {{\"add\": field}}, {{\"drop\": name}} or {{\"rename\": [from, to]}}, not {j}"));
    let o = j.as_object().filter(|o| o.len() == 1).ok_or_else(bad)?;
    match o.iter().next().ok_or_else(bad)? {
        (k, f) if k == "add" => Ok(Alter::AddField(Field::from_short_json(f).map_err(err)?)),
        (k, serde_json::Value::String(name)) if k == "drop" => Ok(Alter::DropField(name.clone())),
        (k, serde_json::Value::Array(xs)) if k == "rename" => match xs.as_slice() {
            [serde_json::Value::String(from), serde_json::Value::String(to)] => Ok(Alter::RenameField { from: from.clone(), to: to.clone() }),
            _ => Err(bad()),
        },
        _ => Err(bad()),
    }
}

fn table_of(db: &Db, table: Option<&str>) -> strata::Result<Arc<Schema>> {
    match table {
        None => Ok(db.catalog().swap_remove(0)),
        Some(t) => db.storage().table(t).ok_or_else(|| strata::Error::Query(format!("no such table: {t}"))),
    }
}

#[pymethods]
impl Database {
    /// Creates a database at `path` with a schema (JSON text, either form).
    #[staticmethod]
    #[pyo3(signature = (path, schema, directory = false, models = None, threads = None))]
    fn create(py: Python<'_>, path: PathBuf, schema: &str, directory: bool, models: Option<PathBuf>, threads: Option<usize>) -> PyResult<Self> {
        let schema = Schema::from_json(schema).map_err(err)?;
        let threads = threads_or_cores(threads)?;
        let opts = db_options(directory, threads);
        let models = models.unwrap_or_else(strata::embed::models_dir);
        let db = py.detach(|| Db::create_in(&path, schema, opts, &models)).map_err(err)?;
        Ok(Database::new(with_threads(db, threads), Some(models)))
    }

    /// Opens the database at `path`.
    #[staticmethod]
    #[pyo3(signature = (path, models = None, threads = None))]
    fn open(py: Python<'_>, path: PathBuf, models: Option<PathBuf>, threads: Option<usize>) -> PyResult<Self> {
        let threads = threads_or_cores(threads)?;
        // The layout comes from the file; the option only matters to a create.
        let db = py.detach(|| Db::open(&path, db_options(false, threads))).map_err(err)?;
        Ok(Database::new(with_threads(db, threads), models))
    }

    /// The schema of the first table, or of `table`, in its short JSON form.
    #[pyo3(signature = (table = None))]
    fn schema_json(&self, table: Option<&str>) -> PyResult<String> {
        Ok(table_of(&*self.get()?, table).map_err(err)?.to_short_json().to_string())
    }

    /// Changes the columns of the first table, or of `table`, in one
    /// change: `alters` is a JSON list of `{"add": field}` (a field in its
    /// short form), `{"drop": name}` and `{"rename": [from, to]}`, applied
    /// in order, as `ALTER TABLE` takes them.
    #[pyo3(signature = (alters, table = None))]
    fn alter_table_json(&self, py: Python<'_>, alters: &str, table: Option<&str>) -> PyResult<()> {
        let db = self.get()?;
        let table = table_of(&db, table).map_err(err)?.table.clone();
        let list: Vec<serde_json::Value> = serde_json::from_str(alters).map_err(|e| PyValueError::new_err(e.to_string()))?;
        let alters = list.iter().map(alter_of).collect::<PyResult<Vec<_>>>()?;
        py.detach(|| db.alter_table(&table, &alters)).map_err(err)
    }

    /// Every table's schema in its short JSON form, the first first.
    fn tables_json(&self) -> PyResult<Vec<String>> {
        Ok(self.get()?.catalog().iter().map(|s| s.to_short_json().to_string()).collect())
    }

    /// Live rows.
    fn count(&self) -> PyResult<u64> {
        Ok(self.get()?.snapshot().n_live_docs())
    }

    /// Runs a SELECT; returns {columns, rows, keys, scores, stats}.
    #[pyo3(signature = (sql, params = None))]
    fn query<'py>(&self, py: Python<'py>, sql: &str, params: Option<&Bound<'py, PyDict>>) -> PyResult<Bound<'py, PyDict>> {
        let db = self.get()?;
        let p = params_json(params)?;
        let rs = py.detach(|| self::params(&db, &p).and_then(|p| db.query(sql, &p))).map_err(err)?;
        result_py(py, &rs)
    }

    /// Runs a SELECT; returns its answer as Arrow columns, which Python
    /// libraries take without copying them (feature `datafusion`).
    #[cfg(feature = "datafusion")]
    #[pyo3(signature = (sql, params = None))]
    fn query_arrow(&self, py: Python<'_>, sql: &str, params: Option<&Bound<'_, PyDict>>) -> PyResult<arrow::Arrow> {
        let db = self.get()?;
        let p = params_json(params)?;
        py.detach(|| self::params(&db, &p).and_then(|p| db.query_leaf(sql, &p)).and_then(arrow::Arrow::of)).map_err(err)
    }

    /// Runs any statement: a SELECT's answer as `query` gives it, or the
    /// number of rows an INSERT, UPDATE or DELETE wrote.
    #[pyo3(signature = (sql, params = None))]
    fn sql<'py>(&self, py: Python<'py>, sql: &str, params: Option<&Bound<'py, PyDict>>) -> PyResult<Bound<'py, PyAny>> {
        let db = self.get()?;
        let p = params_json(params)?;
        match py.detach(|| self::params(&db, &p).and_then(|p| db.sql(sql, &p))).map_err(err)? {
            SqlResult::Rows(rs) => Ok(result_py(py, &rs)?.into_any()),
            SqlResult::Written(n) => Ok(n.into_pyobject(py)?.into_any()),
        }
    }

    /// Runs a SELECT and logs what it served (`log`: the options as JSON
    /// text, as strata-server's `"log"` takes them); returns its answer as
    /// `query` does, with the request's id and an interleaved query's
    /// teams.
    fn query_logged<'py>(&self, py: Python<'py>, sql: &str, params: Option<&Bound<'py, PyDict>>, log: &str) -> PyResult<Bound<'py, PyDict>> {
        let db = self.get()?;
        let p = params_json(params)?;
        let log: Json = serde_json::from_str(log).map_err(|e| StrataError::new_err(format!("log options are JSON: {e}")))?;
        let l = py.detach(|| Log::from_json(&log).and_then(|log| self::params(&db, &p).and_then(|p| db.query_logged(sql, &p, &log)))).map_err(err)?;
        let d = result_py(py, &l.rs)?;
        d.set_item("request", l.request)?;
        if !l.teams.is_empty() {
            d.set_item("teams", l.teams)?;
        }
        Ok(d)
    }

    /// The records of a log table (`export`: the options as JSON text, as
    /// strata-server's `/v1/rank_log/export` takes them), as JSON text of a
    /// list.
    fn export_rank_log(&self, py: Python<'_>, export: &str) -> PyResult<String> {
        let db = self.get()?;
        let x: Json = serde_json::from_str(export).map_err(|e| StrataError::new_err(format!("export options are JSON: {e}")))?;
        py.detach(|| {
            let x = Export::from_json(&x)?;
            let mut out = Vec::new();
            db.export_rank_log(&x, |r| Ok(out.push(r)))?;
            Ok(Json::Array(out).to_string())
        })
        .map_err(err)
    }

    /// The plan of a SELECT, INSERT, UPDATE or DELETE as JSON text, without
    /// running it.
    #[pyo3(signature = (sql, params = None))]
    fn plan(&self, py: Python<'_>, sql: &str, params: Option<&Bound<'_, PyDict>>) -> PyResult<String> {
        let db = self.get()?;
        let p = params_json(params)?;
        py.detach(|| self::params(&db, &p).and_then(|p| db.explain(sql, &p)).and_then(|plan| plan.to_json_text())).map_err(err)
    }

    /// Runs a plan given as JSON text, as `sql` runs a statement.
    fn run<'py>(&self, py: Python<'py>, plan: &str) -> PyResult<Bound<'py, PyAny>> {
        let db = self.get()?;
        let plan: Json = serde_json::from_str(plan).map_err(|e| StrataError::new_err(format!("a plan is JSON: {e}")))?;
        match py.detach(|| Plan::from_json(&plan).and_then(|plan| db.run(&plan))).map_err(err)? {
            SqlResult::Rows(rs) => Ok(result_py(py, &rs)?.into_any()),
            SqlResult::Written(n) => Ok(n.into_pyobject(py)?.into_any()),
        }
    }

    /// Inserts rows (keys must be new) into the first table or `table`;
    /// all or none are written.
    #[pyo3(signature = (rows, table = None))]
    fn insert(&self, py: Python<'_>, rows: &Bound<'_, PyAny>, table: Option<&str>) -> PyResult<u64> {
        let db = self.get()?;
        let schema = table_of(&db, table).map_err(err)?;
        let rows = rows_of(rows, &schema)?;
        py.detach(|| -> strata::Result<u64> {
            let docs = Database::documents(&db, &schema, rows, 1)?;
            let n = docs.len() as u64;
            if docs.len() >= BULK_INSERT {
                db.load_into(&schema.table, docs)?;
            } else {
                // Threads inserting at once share commits.
                db.write_ops(docs.into_iter().map(|doc| WriteOp::Insert { table: schema.table.clone(), doc }).collect())?;
            }
            Ok(n)
        })
        .map_err(err)
    }

    /// Inserts rows or replaces the rows with their keys, in the first
    /// table or `table`, in transactions of 10,000 rows.
    #[pyo3(signature = (rows, table = None))]
    fn upsert(&self, py: Python<'_>, rows: &Bound<'_, PyAny>, table: Option<&str>) -> PyResult<u64> {
        let db = self.get()?;
        let schema = table_of(&db, table).map_err(err)?;
        let rows = rows_of(rows, &schema)?;
        py.detach(|| -> strata::Result<u64> {
            let mut n = 0;
            let mut rows = rows.into_iter().peekable();
            while rows.peek().is_some() {
                let chunk: Vec<Row> = rows.by_ref().take(BATCH).collect();
                let len = chunk.len() as u64;
                let docs = Database::documents(&db, &schema, chunk, n as usize + 1)?;
                db.write_ops(docs.into_iter().map(|doc| WriteOp::Upsert { table: schema.table.clone(), doc }).collect())?;
                n += len;
            }
            Ok(n)
        })
        .map_err(err)
    }

    /// Deletes the rows with these keys from the first table or `table`;
    /// returns how many there were.
    #[pyo3(signature = (keys, table = None))]
    fn delete(&self, py: Python<'_>, keys: &Bound<'_, PyAny>, table: Option<&str>) -> PyResult<u64> {
        let db = self.get()?;
        let keys = if keys.cast::<PyString>().is_ok() || keys.cast::<PyInt>().is_ok() { vec![to_json(keys)?] } else { rows_json(keys)? };
        py.detach(|| -> strata::Result<u64> {
            let s = table_of(&db, table)?;
            let pk = s.primary_key.ok_or_else(|| strata::Error::Query("deleting by key needs a primary key".into()))?;
            let ops = keys.iter().map(|k| Ok(WriteOp::Delete { table: s.table.clone(), key: ingest::value(&s, pk, k)? })).collect::<strata::Result<Vec<_>>>()?;
            db.write_ops(ops)
        })
        .map_err(err)
    }

    /// The rows with these keys of the first table or `table`, read by
    /// key without SQL ([`Db::get_many`]): per key, in their order, a dict
    /// of the columns `SELECT *` answers, or None for a key no live row
    /// has (or whose row has expired).
    #[pyo3(signature = (keys, table = None))]
    fn get_many<'py>(&self, py: Python<'py>, keys: &Bound<'py, PyAny>, table: Option<&str>) -> PyResult<Bound<'py, PyList>> {
        let db = self.get()?;
        let keys = rows_json(keys)?;
        let (columns, rows) = py
            .detach(|| -> strata::Result<_> {
                let s = table_of(&db, table)?;
                let pk = s.primary_key.ok_or_else(|| strata::Error::Query(format!("{} has no primary key: rows are read by key only from a table with one", s.table)))?;
                let keys = keys.iter().map(|k| ingest::value(&s, pk, k)).collect::<strata::Result<Vec<_>>>()?;
                Ok((Db::get_columns(&s), db.get_many(&s.table, &keys)?))
            })
            .map_err(err)?;
        let out = rows
            .iter()
            .map(|r| match r {
                None => Ok(py.None().into_bound(py)),
                Some(row) => {
                    let d = PyDict::new(py);
                    for (c, v) in columns.iter().zip(row) {
                        d.set_item(c, to_py(py, v)?)?;
                    }
                    Ok(d.into_any())
                }
            })
            .collect::<PyResult<Vec<_>>>()?;
        PyList::new(py, out)
    }

    /// Deletes the rows whose `expire_by` time has passed, in every table
    /// that has one ([`Db::expire`]); returns how many. Queries and reads
    /// by key leave such rows out before this deletes them.
    fn expire(&self, py: Python<'_>) -> PyResult<u64> {
        let db = self.get()?;
        py.detach(|| db.expire(strata::timestamp::now(), 10_000)).map_err(err)
    }

    /// Applies write operations as strata-server's /v1/write takes them, in
    /// one transaction: insert, upsert and delete, set of a field (a
    /// number in place, another by writing the row again), and add to a
    /// number in place (an add with a "doc" inserts it when no row has its
    /// key). Returns the rows written.
    fn write(&self, py: Python<'_>, ops: &Bound<'_, PyAny>) -> PyResult<u64> {
        let db = self.get()?;
        let ops = rows_json(ops)?;
        py.detach(|| -> strata::Result<u64> {
            let bad = |i: usize, m: &str| strata::Error::Query(format!("operation {}: {m}", i + 1));
            // Read before the write starts: threads writing at once share
            // its commit.
            let mut write = Vec::with_capacity(ops.len());
            for (i, op) in ops.iter().enumerate() {
                let get = |k: &str| op.get(k).filter(|v| !v.is_null());
                let kind = get("op").and_then(Json::as_str).ok_or_else(|| bad(i, "it needs \"op\""))?;
                let s = table_of(&db, get("table").and_then(Json::as_str))?;
                let table = s.table.clone();
                let doc = || match get("doc") {
                    Some(d) => Ok(Database::documents(&db, &s, vec![Row { json: d.clone(), vectors: Vec::new() }], 1)?.remove(0)),
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
                                return Err(bad(i, "add's key and its doc's key differ: give one"));
                            }
                            WriteOp::AddOrInsert { table, doc: d, field: field()?, delta }
                        } else {
                            WriteOp::Add { table, key: key()?, field: field()?, delta }
                        }
                    }
                    other => return Err(bad(i, &format!("unknown op {other:?}"))),
                });
            }
            db.write_ops(write)
        })
        .map_err(err)
    }

    /// Loads a JSON Lines, JSON, CSV or TSV file; returns (rows, bulk,
    /// skipped fields).
    #[pyo3(signature = (path, format = None, table = None))]
    fn import_file(&self, py: Python<'_>, path: PathBuf, format: Option<&str>, table: Option<&str>) -> PyResult<(u64, bool, Vec<String>)> {
        let db = self.get()?;
        let format = format.map(|f| Format::parse(f).ok_or_else(|| PyValueError::new_err(format!("unknown format {f:?}: jsonl, json, csv or tsv")))).transpose()?;
        let r = py.detach(|| local(&path).and_then(|l| table_of(&db, table).and_then(|s| ingest::import_file_into(&db, &s.table, &l.path, format)))).map_err(err)?;
        Ok((r.rows, r.bulk, r.unknown))
    }

    /// Imports the tables of the SQLite file `path` ([`sqlite::import`]);
    /// returns what it did as JSON.
    #[pyo3(signature = (path, tables = None, schemas = None, blobs_base64 = false))]
    fn import_sqlite(&self, py: Python<'_>, path: PathBuf, tables: Option<Vec<String>>, schemas: Option<&str>, blobs_base64: bool) -> PyResult<String> {
        let db = self.get()?;
        let opts = Database::sqlite_options(tables, schemas, blobs_base64)?;
        let r = py.detach(|| local(&path).and_then(|l| sqlite::import(&db, &l.path, &opts))).map_err(err)?;
        Ok(r.to_json().to_string())
    }

    /// Writes the tables to a new SQLite file ([`sqlite::export`]); returns
    /// what it wrote as JSON.
    #[pyo3(signature = (path, tables = None, fts = true, indexes = true, schemas = true))]
    fn export_sqlite(&self, py: Python<'_>, path: PathBuf, tables: Option<Vec<String>>, fts: bool, indexes: bool, schemas: bool) -> PyResult<String> {
        let db = self.get()?;
        let opts = sqlite::ExportOptions { tables: tables.unwrap_or_default(), fts, indexes, meta: schemas };
        let r = py
            .detach(|| {
                let out = remote(&path)?;
                let r = sqlite::export(&db, &out.path, &opts)?;
                out.finish()?;
                Ok(r)
            })
            .map_err(err)?;
        Ok(r.to_json().to_string())
    }

    /// Imports the Parquet file or directory `path` (or `s3://` URL) into
    /// one table ([`strata::parquet::import`]); returns what it did as JSON.
    #[cfg(feature = "parquet")]
    #[pyo3(signature = (path, table = None, schema = None, key = None, blobs_base64 = false))]
    fn import_parquet(&self, py: Python<'_>, path: PathBuf, table: Option<String>, schema: Option<&str>, key: Option<String>, blobs_base64: bool) -> PyResult<String> {
        let db = self.get()?;
        let opts = parquet_options(table, schema, key, blobs_base64)?;
        let r = py.detach(|| local(&path).and_then(|l| strata::parquet::import(&db, &l.path, &opts))).map_err(err)?;
        Ok(r.to_json().to_string())
    }

    /// Writes the tables to Parquet ([`strata::parquet::export`]): a
    /// `.parquet` file for one table, else a directory of them; `path` may
    /// be an `s3://` URL. Returns what it wrote as JSON.
    #[cfg(feature = "parquet")]
    #[pyo3(signature = (path, tables = None, schemas = true))]
    fn export_parquet(&self, py: Python<'_>, path: PathBuf, tables: Option<Vec<String>>, schemas: bool) -> PyResult<String> {
        let db = self.get()?;
        let opts = strata::parquet::ExportOptions { tables: tables.unwrap_or_default(), meta: schemas };
        let r = py
            .detach(|| {
                let out = remote(&path)?;
                let r = strata::parquet::export(&db, &out.path, &opts)?;
                out.finish()?;
                Ok(r)
            })
            .map_err(err)?;
        Ok(r.to_json().to_string())
    }

    /// Writes the rows of an Arrow stream (`__arrow_c_stream__`) into the
    /// first table or `table`, columns filling the fields of their names
    /// ([`strata::arrow_rows::import_batches`]); returns what it did as
    /// JSON.
    #[cfg(feature = "parquet")]
    #[pyo3(signature = (data, table = None))]
    fn import_arrow(&self, py: Python<'_>, data: &Bound<'_, PyAny>, table: Option<&str>) -> PyResult<String> {
        use arrow_array::RecordBatchReader;
        let db = self.get()?;
        let reader = arrow::stream_of(data)?;
        let table = table.map(String::from);
        let w = py
            .detach(move || -> strata::Result<_> {
                let schema = table_of(&db, table.as_deref())?;
                let arrow = reader.schema();
                let mut batches = reader.map(|b| b.map_err(|e| strata::Error::Schema(format!("the Arrow stream: {e}"))));
                let w = strata::arrow_rows::import_batches(&db, &schema.table, &arrow, &mut batches)?;
                Ok((schema.table.clone(), w))
            })
            .map_err(err)?;
        let (table, w) = w;
        Ok(serde_json::json!({"table": table, "rows": w.rows, "bulk": w.bulk, "skipped": w.skipped, "notes": w.notes}).to_string())
    }

    /// Creates a database at `path` from the tables of the SQLite file
    /// `sqlite_path`; returns it and what the import did as JSON.
    #[staticmethod]
    #[pyo3(signature = (sqlite_path, path, tables = None, schemas = None, blobs_base64 = false, directory = false, models = None, threads = None))]
    #[allow(clippy::too_many_arguments)]
    fn from_sqlite(
        py: Python<'_>,
        sqlite_path: PathBuf,
        path: PathBuf,
        tables: Option<Vec<String>>,
        schemas: Option<&str>,
        blobs_base64: bool,
        directory: bool,
        models: Option<PathBuf>,
        threads: Option<usize>,
    ) -> PyResult<(Self, String)> {
        let opts = Database::sqlite_options(tables, schemas, blobs_base64)?;
        let threads = threads_or_cores(threads)?;
        let db_opts = db_options(directory, threads);
        let models = models.unwrap_or_else(strata::embed::models_dir);
        let (db, embed_errors, report) = py
            .detach(|| -> strata::Result<_> {
                let l = local(&sqlite_path)?;
                let src = sqlite::Source::open(&l.path)?;
                let plan = src.plan(None, &opts)?;
                let first = plan.tables.first().ok_or_else(|| strata::Error::Schema(format!("{}: no table to import", sqlite_path.display())))?;
                let db = Db::create_in(&path, first.schema.clone(), db_opts, &models)?;
                // Before the rows, so that they get their embeddings.
                let embed_errors: Vec<(String, String)> = db.open_embedders(&models).into_iter().map(|(f, e)| (f, e.to_string())).collect();
                let r = src.load(&db, &plan)?;
                Ok((db, embed_errors, r.to_json().to_string()))
            })
            .map_err(err)?;
        Ok((Database::of(with_threads(db, threads), embed_errors), report))
    }

    /// What an import of the SQLite file `sqlite_path` into a new database
    /// would do ([`sqlite::Plan::to_json`]).
    #[staticmethod]
    #[pyo3(signature = (sqlite_path, tables = None, schemas = None, blobs_base64 = false))]
    fn plan_sqlite(py: Python<'_>, sqlite_path: PathBuf, tables: Option<Vec<String>>, schemas: Option<&str>, blobs_base64: bool) -> PyResult<String> {
        let opts = Database::sqlite_options(tables, schemas, blobs_base64)?;
        let plan = py.detach(|| local(&sqlite_path).and_then(|l| sqlite::plan(&l.path, None, &opts))).map_err(err)?;
        Ok(plan.to_json().to_string())
    }

    /// Creates a database at `path` whose first table is the one the
    /// Parquet file or directory `parquet_path` (or `s3://` URL) makes, and
    /// loads it ([`strata::parquet::import_new`]); returns it and what the
    /// import did as JSON.
    #[cfg(feature = "parquet")]
    #[staticmethod]
    #[pyo3(signature = (parquet_path, path, table = None, schema = None, key = None, blobs_base64 = false, directory = false, models = None))]
    #[allow(clippy::too_many_arguments)]
    fn from_parquet(
        py: Python<'_>,
        parquet_path: PathBuf,
        path: PathBuf,
        table: Option<String>,
        schema: Option<&str>,
        key: Option<String>,
        blobs_base64: bool,
        directory: bool,
        models: Option<PathBuf>,
    ) -> PyResult<(Self, String)> {
        let opts = parquet_options(table, schema, key, blobs_base64)?;
        let db_opts = DbOptions { layout: if directory { Layout::Directory } else { Layout::SingleFile }, ..DbOptions::default() };
        let models = models.unwrap_or_else(strata::embed::models_dir);
        let (db, embed_errors, report) = py
            .detach(|| -> strata::Result<_> {
                let l = local(&parquet_path)?;
                let src = strata::parquet::Source::open(&l.path)?;
                let plan = src.plan(None, &opts)?;
                let db = Db::create_in(&path, plan.schema.clone(), db_opts, &models)?;
                // Before the rows, so that they get their embeddings.
                let embed_errors: Vec<(String, String)> = db.open_embedders(&models).into_iter().map(|(f, e)| (f, e.to_string())).collect();
                let r = src.load(&db, &plan)?;
                Ok((db, embed_errors, r.to_json().to_string()))
            })
            .map_err(err)?;
        Ok((Database::of(db, embed_errors), report))
    }

    /// What an import of the Parquet file or directory `parquet_path` (or
    /// `s3://` URL) would do: into this database's tables with `self`, else
    /// into a new database ([`strata::parquet::Plan::to_json`]).
    #[cfg(feature = "parquet")]
    #[staticmethod]
    #[pyo3(signature = (parquet_path, table = None, schema = None, key = None, blobs_base64 = false, db = None))]
    fn plan_parquet(py: Python<'_>, parquet_path: PathBuf, table: Option<String>, schema: Option<&str>, key: Option<String>, blobs_base64: bool, db: Option<PyRef<'_, Database>>) -> PyResult<String> {
        let opts = parquet_options(table, schema, key, blobs_base64)?;
        let db = db.map(|d| d.get()).transpose()?;
        let plan = py.detach(|| local(&parquet_path).and_then(|l| strata::parquet::plan(&l.path, db.as_deref(), &opts))).map_err(err)?;
        Ok(plan.to_json().to_string())
    }

    /// The vector `field`'s model gives `text` as a query, or `image` or
    /// `audio`, the bytes of an image or sound file, for a model of that.
    #[pyo3(signature = (text = None, field = None, table = None, image = None, audio = None))]
    fn embed(&self, py: Python<'_>, text: Option<&str>, field: Option<&str>, table: Option<&str>, image: Option<Vec<u8>>, audio: Option<Vec<u8>>) -> PyResult<Vec<f32>> {
        use strata::embed::Modality;
        let db = self.get()?;
        let file = match (image, audio) {
            (Some(_), Some(_)) => return Err(PyValueError::new_err("embed takes a text, an image or a sound, one of them")),
            (Some(d), None) => Some((Modality::Image, d)),
            (None, Some(d)) => Some((Modality::Audio, d)),
            (None, None) => None,
        };
        if let Some((m, data)) = file {
            let s = table_of(&db, table).map_err(err)?;
            let what = strata::embed::modality_name(m);
            let field = match field {
                Some(f) => f.to_string(),
                None => s.fields.iter().find(|f| db.embedder_in(&s.table, &f.name).is_some_and(|e| e.modalities().contains(&m))).map(|f| f.name.clone()).ok_or_else(|| PyValueError::new_err(format!("no embedded field has a model of {what} open")))?,
            };
            let e = db.embedder_in(&s.table, &field).ok_or_else(|| PyValueError::new_err(format!("{field} has no embedding model open (see Database.open's models)")))?;
            strata::embed::file_is(m, &data).map_err(err)?;
            return py.detach(|| strata::embed::embed_file(e.as_ref(), &data)).map_err(err);
        }
        let text = text.ok_or_else(|| PyValueError::new_err("embed takes a text, an image or a sound"))?;
        let mut o = Map::new();
        o.insert("embed".into(), Json::String(text.into()));
        if let Some(f) = field {
            o.insert("field".into(), Json::String(f.into()));
        }
        if let Some(t) = table {
            o.insert("table".into(), Json::String(t.into()));
        }
        match py.detach(|| param(&db, &Json::Object(o))).map_err(err)? {
            Value::Vector(v) => Ok(v.to_vec()),
            _ => Err(PyValueError::new_err("the field's model embeds a text a vector a token (a multi-vector field): Database.embed_tokens gives them")),
        }
    }

    /// The vectors of `text`'s tokens as a query, by the model of
    /// multi-vector field `field` (ColBERT), as `maxsim` takes them
    /// ([`Db::embed_query_in`]).
    #[pyo3(signature = (text, field, table = None))]
    fn embed_tokens(&self, py: Python<'_>, text: &str, field: &str, table: Option<&str>) -> PyResult<Vec<Vec<f32>>> {
        let db = self.get()?;
        let table = table_of(&db, table).map_err(err)?.table.clone();
        match py.detach(|| db.embed_query_in(&table, field, text)).map_err(err)? {
            Value::List(vs) => Ok(vs.iter().filter_map(|v| if let Value::Vector(v) = v { Some(v.to_vec()) } else { None }).collect()),
            _ => Err(PyValueError::new_err(format!("{field} is a field of one vector: Database.embed gives it"))),
        }
    }

    /// The codes that can follow each of `prefixes` in semantic-ID field
    /// `field` of `table` (the first by default) among the rows SQL
    /// condition `filter` matches, with their rows ([`Db::sid_next`]).
    #[pyo3(signature = (field, prefixes, filter = None, params = None, table = None))]
    fn sid_next(&self, py: Python<'_>, field: &str, prefixes: Vec<String>, filter: Option<String>, params: Option<&Bound<'_, PyDict>>, table: Option<&str>) -> PyResult<Vec<Vec<(String, u64)>>> {
        let db = self.get()?;
        let p = params_json(params)?;
        let table = table_of(&db, table).map_err(err)?.table.clone();
        py.detach(|| self::params(&db, &p).and_then(|p| db.sid_next(&table, field, &prefixes, filter.as_deref(), &p))).map_err(err)
    }

    /// Trains a semantic-ID codebook on the vectors of `field` of `table`
    /// (the first table by default), saves it as `name` under `models`, and
    /// returns its SHA-256.
    #[pyo3(signature = (field, name, table = None, models = None, levels = 3, k = 256, iterations = 25, sample = None, seed = 0))]
    #[allow(clippy::too_many_arguments)]
    fn train_codebook(&self, py: Python<'_>, field: &str, name: &str, table: Option<String>, models: Option<PathBuf>, levels: usize, k: usize, iterations: usize, sample: Option<usize>, seed: u64) -> PyResult<String> {
        let db = self.get()?;
        let table = table.unwrap_or_else(|| db.schema().table.clone());
        let models = models.unwrap_or_else(strata::embed::models_dir);
        let opts = train_options(levels, k, iterations, sample, seed);
        py.detach(|| db.train_codebook(&table, field, name, &models, &opts)).map_err(err)
    }

    /// Writes the memtable's rows into segments, merges them into as few
    /// as it can and vacuums the file ([`Db::optimize`]).
    fn optimize(&self, py: Python<'_>) -> PyResult<()> {
        let db = self.get()?;
        py.detach(|| db.optimize().map(drop)).map_err(err)
    }

    /// Returns the space merges freed to the file system; the bytes the
    /// file shrank by.
    fn vacuum(&self, py: Python<'_>) -> PyResult<u64> {
        let db = self.get()?;
        py.detach(|| db.storage().vacuum()).map_err(err)
    }

    /// Merges small segments, gives the file system back the space merges
    /// freed when it is an eighth of the file or more ([`Db::close`]), and
    /// closes the database. It also closes when the last reference to it
    /// goes, without the merge.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let db = self.db.write().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(db) = db {
            py.detach(|| db.close().map(drop)).map_err(err)?;
        }
        Ok(())
    }

    fn closed(&self) -> bool {
        self.db.read().unwrap_or_else(|e| e.into_inner()).is_none()
    }
}

fn train_options(levels: usize, k: usize, iters: usize, sample: Option<usize>, seed: u64) -> TrainOptions {
    TrainOptions { levels, k, iters, sample: sample.unwrap_or(TrainOptions::default().sample), seed }
}

/// Trains a semantic-ID codebook on `vectors` (a list of vectors, or
/// anything with `tolist()`), saves it as `name` under `models`, and
/// returns its SHA-256.
#[pyfunction]
#[pyo3(signature = (vectors, name, models = None, levels = 3, k = 256, iterations = 25, sample = None, seed = 0))]
#[allow(clippy::too_many_arguments)]
fn train_codebook(py: Python<'_>, vectors: &Bound<'_, PyAny>, name: &str, models: Option<PathBuf>, levels: usize, k: usize, iterations: usize, sample: Option<usize>, seed: u64) -> PyResult<String> {
    let Json::Array(rows) = to_json(vectors)? else { return Err(PyValueError::new_err("vectors are a list of lists of numbers")) };
    let dim = match rows.first() {
        Some(Json::Array(v)) => v.len(),
        _ => return Err(PyValueError::new_err("vectors are a list of lists of numbers, at least one")),
    };
    let mut flat = Vec::with_capacity(rows.len() * dim);
    for (i, r) in rows.iter().enumerate() {
        match r {
            Json::Array(v) if v.len() == dim => {
                for x in v {
                    flat.push(x.as_f64().ok_or_else(|| PyValueError::new_err(format!("vector {}: {x} is not a number", i + 1)))? as f32);
                }
            }
            _ => return Err(PyValueError::new_err(format!("vector {} does not have {dim} numbers, as the first does", i + 1))),
        }
    }
    let models = models.unwrap_or_else(strata::embed::models_dir);
    let opts = train_options(levels, k, iterations, sample, seed);
    py.detach(|| Codebook::train(&flat, dim, &opts)?.save(name, &models)).map_err(err)
}

/// Copies the database at `path`, written by a version of Strata from
/// before field ids (which no longer opens), into this version's format in
/// its place, keeping the old one as `<path>.before-field-ids`. Returns
/// `None` when it needs no upgrade, else the old database's path, the
/// tables copied and their rows.
#[pyfunction]
fn upgrade(py: Python<'_>, path: PathBuf) -> PyResult<Option<(PathBuf, usize, u64)>> {
    Ok(py.detach(|| strata::upgrade::upgrade(&path)).map_err(err)?.map(|u| (u.old, u.tables, u.rows)))
}

#[pymodule]
fn _strata(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Database>()?;
    #[cfg(feature = "datafusion")]
    m.add_class::<arrow::Arrow>()?;
    m.add_function(wrap_pyfunction!(train_codebook, m)?)?;
    m.add_function(wrap_pyfunction!(upgrade, m)?)?;
    m.add("StrataError", m.py().get_type::<StrataError>())?;
    Ok(())
}
