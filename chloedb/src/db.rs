//! The database: storage plus the query engine, behind SQL.
//!
//! ```no_run
//! # use chloedb::{db::Db, query::Params, storage::DbOptions, data};
//! let db = Db::open(std::path::Path::new("papers.chloedb"), DbOptions::default()).unwrap();
//! let rows = db.query("SELECT id, score() FROM docs ORDER BY bm25(body, 'mrna vaccine') DESC LIMIT 10", &Params::new()).unwrap();
//! ```
//!
//! SQL reads and writes every table of the database by name, and creates
//! and drops tables (`CREATE TABLE`, `DROP TABLE`); a query reads one table.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::derive::Deriver;
use crate::embed::{Embedder, Input, Modality};
use crate::error::{Error, Result};
use crate::forward::{DocStore, KeywordColumn, NumericColumn};
use crate::media::{Media, Resolver};
use crate::query::agg::Part;
use crate::query::exec::{ExecOptions, ExecStats, Executor, Leaf, ResultSet, Scroll, SegmentReaders};
use crate::query::ir::eval::{self, Ref};
use crate::query::ir::Plan;
use crate::query::model::Models;
use crate::query::stats::TermStats;
use crate::query::{bind_command, compound, ops, profile, sql as qsql, Assign, Command, Conflict, Params, Select, Write};
use crate::rank_log::{self, Export, Log, Logged};
use crate::schema::{Alter, Derive, Embed, FieldType, Schema};
use crate::segment::{DocId, SegmentFile};
use crate::query::function::{self, FunctionSet};
use crate::storage::{Applied, Database, DbOptions, DocumentSource, DropOptions, Found, Mask, MergePolicy, MergeStats, SeqNo, Snapshot, TableTxn, WriteTxn, FUNCTIONS, SOURCES};
use crate::value::{Document, Value};
use crate::vector::VectorReader;

mod recommend;
mod rollup;
mod writes;
pub use recommend::{RecommendSql, Recommended};
pub use rollup::RefreshStats;
pub use writes::{Footprints, WriteOp, WriteStats, GROUP_OPS};

/// What [`Db::sql`] returns.
#[derive(Debug)]
pub enum SqlResult {
    Rows(ResultSet),
    /// Documents inserted, updated or deleted (0 for `CREATE TABLE` and
    /// `DROP TABLE`).
    Written(u64),
}

/// The end of the error of a write whose table was altered (or dropped and
/// created again) after the write was bound and before it ran
/// ([`Db::write_prepared`]): bound again, it runs.
const ALTERED: &str = "while the statement ran; run it again";

/// How many times a write whose table was altered as it ran is bound again
/// and run ([`altered_while_running`]) before its error is answered: each
/// time lost is an `ALTER TABLE` that committed meanwhile.
pub const ALTERED_TRIES: usize = 10;

/// Whether the error `message` says that the write's table was altered
/// after the write was bound, so that the same statement bound again may
/// run: an `ALTER TABLE` raced it.
pub fn altered_while_running(message: &str) -> bool {
    message.contains(ALTERED)
}

/// Refuses a derived field of table `table` whose dictionary
/// (`dict:<table>.<field>`) is a field of that table itself: every write
/// changes the dictionary it reads, so each would read the table's values
/// again before it is written (audit 4, X-C2). Checked when a table is
/// made or a column added, so that a database that holds one still opens.
fn no_self_dictionary(table: &str, f: &crate::schema::Field) -> Result<()> {
    let Some(d) = f.derive.as_ref().filter(|d| d.model.starts_with(crate::content::DICT)) else { return Ok(()) };
    match crate::content::DictEntities::new(&d.model, crate::text::Analyzer::default()) {
        Ok(dict) if dict.table.eq_ignore_ascii_case(table) => Err(Error::Schema(format!(
            "field {}: model {} is a dictionary of {table} itself, which each write to {table} would read again: keep the names in a table of their own (CREATE TABLE names (name KEYWORD PRIMARY KEY)) and name it",
            f.name, d.model
        ))),
        _ => Ok(()),
    }
}

/// An INSERT, UPDATE or DELETE bound to its table, with the vectors it
/// writes computed ([`Db::prepare_write`]), to run with
/// [`Db::write_prepared`].
#[derive(Clone, Debug)]
pub struct PreparedWrite {
    schema: Arc<Schema>,
    write: Write,
    known: Known,
    derived: Derived,
    /// Rows an UPDATE or DELETE passes over ([`PreparedWrite::masked`]).
    mask: Option<Arc<Mask>>,
    /// Asked as the write runs ([`PreparedWrite::guarded`]).
    guard: Option<Guard>,
    /// An UPDATE that fills in the vectors of this embedded field
    /// ([`Db::prepare_backfill`]): it passes over the rows that have one
    /// when it runs, and those whose sources are all empty.
    backfill: Option<usize>,
}

/// A check a write passes as it runs, under the writer lock, answering the
/// rows an UPDATE or DELETE passes over then ([`PreparedWrite::guarded`]).
#[derive(Clone)]
struct Guard(Arc<dyn Fn() -> Result<Option<Arc<Mask>>> + Send + Sync>);

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Guard")
    }
}

impl PreparedWrite {
    /// The primary keys of the documents an INSERT writes; none for an
    /// UPDATE or a DELETE, which change rows already in the table (and
    /// cannot change a key).
    pub fn inserted_keys(&self) -> Vec<&Value> {
        match (&self.write, self.schema.primary_key) {
            (Write::Insert { docs, .. }, Some(pk)) => docs.iter().map(|d| &d[pk]).collect(),
            _ => Vec::new(),
        }
    }

    /// The values that place the documents an INSERT writes on shards:
    /// their keys ([`Self::inserted_keys`]), or for a table placed by a
    /// column ([`Schema::with_shard_by`]) that column's values.
    pub fn inserted_places(&self) -> Vec<&Value> {
        match (&self.write, self.schema.shard_by_field()) {
            (Write::Insert { docs, .. }, Some(f)) => docs.iter().map(|d| &d[f]).collect(),
            _ => self.inserted_keys(),
        }
    }

    /// The table it writes.
    pub fn table(&self) -> &Schema {
        &self.schema
    }

    /// The same UPDATE or DELETE passing over the rows `mask` hides
    /// ([`Snapshot::masked`]): a shard runs a statement over some of its
    /// slots so. Refused for an INSERT, whose rows are its own.
    pub fn masked(mut self, mask: Arc<Mask>) -> Result<PreparedWrite> {
        if matches!(self.write, Write::Insert { .. }) {
            return Err(Error::Query("an INSERT writes its own rows: only an UPDATE or DELETE passes over some".into()));
        }
        self.mask = Some(mask);
        Ok(self)
    }

    /// The same write running only if `guard` passes, asked under the
    /// writer lock as the write starts; an UPDATE or DELETE passes over
    /// the rows of the mask it answers too (an INSERT's rows are its own).
    /// A shard checks so that the rows it holds without serving, and the
    /// slots it serves, are still those the statement was routed by.
    pub fn guarded(mut self, guard: impl Fn() -> Result<Option<Arc<Mask>>> + Send + Sync + 'static) -> PreparedWrite {
        self.guard = Some(Guard(Arc::new(guard)));
        self
    }
}

/// A row read by its primary key ([`Db::get_many_at`]): the columns of
/// [`Db::get_columns`], typed as a query's answer types them, and the sequence
/// number of the write that last changed it (0 in a database that does
/// not number its writes, and for rows from before it did).
#[derive(Clone, Debug, PartialEq)]
pub struct KeyRow {
    pub row: Vec<Value>,
    pub seq_no: u64,
}

/// `key` as a value of primary key `pk` of `schema`: a KEYWORD key is
/// text, and an INT key an integer or text of one.
pub fn key_value(schema: &Schema, pk: usize, key: &Value) -> Result<Value> {
    let f = &schema.fields[pk];
    match (&f.ty, key) {
        (FieldType::Keyword, Value::Str(_)) | (FieldType::Int, Value::Int(_)) => Ok(key.clone()),
        (FieldType::Int, Value::Str(s)) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| Error::Query(format!("{s:?} is not a key of {}: its key {} is an INT", schema.table, f.name))),
        _ => Err(Error::Query(format!("{} is not a key of {}: its key {} is {}", crate::query::hint::sql_value(key), schema.table, f.name, if f.ty == FieldType::Int { "an INT" } else { "a KEYWORD" }))),
    }
}

/// Values of embedded fields computed ahead of a write (a VECTOR field's
/// vectors, a SPARSE field's term weights), by field and source text.
type Known = HashMap<usize, HashMap<String, Value>>;

/// Text made ahead of a write, by derived field and the MEDIA value it is
/// made from.
type Derived = HashMap<usize, HashMap<String, String>>;

/// A stream's source, as the system table [`SOURCES`] keeps it: the
/// offset of the last event written from it, and when that was written
/// (microseconds since the epoch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub offset: i64,
    pub updated: Option<i64>,
}

pub struct Db {
    storage: Arc<Database>,
    /// Readers by segment id, with the address of the image they read.
    /// On-disk segments keep their id for life (deletes travel with the
    /// snapshot, not the readers), but a vacuum can move their bytes;
    /// in-memory ones get a fresh id per rebuild.
    readers: RwLock<HashMap<u64, (usize, Arc<Schema>, Arc<SegmentReaders>)>>,
    /// The storage's [`Database::image_generation`] the cache last saw: once
    /// it changes, readers of images no snapshot holds are dropped, so that
    /// the files of segments made cold or evicted can go.
    readers_gen: std::sync::atomic::AtomicU64,
    /// The options [`Db::query`], [`Db::sql`] and [`Db::run`] run with,
    /// and that [`Db::query_with`]'s callers start from. They count each
    /// query against the memory budget of the process
    /// ([`crate::query::memory::process_budget`]: a quarter of the
    /// machine's memory, or `CHLOEDB_QUERY_MEMORY_MB`), so that a query
    /// that would hold more fails with `Error::Memory` instead of taking
    /// the program's memory; `exec.memory = None` counts nothing. They set
    /// no deadline, no row window (`max_rows`) and no group cap
    /// (`max_groups`): a query given `deadline` or `cancel`
    /// ([`ExecOptions::limited`]) stops within a few thousand rows of it.
    pub exec: ExecOptions,
    /// Models of embedded VECTOR fields ([`Db::set_embedder_in`]).
    embedders: RwLock<Vec<Model>>,
    /// Models queries named in `embed('model', ..)`, by name, opened from
    /// [`Db::models_dir`] at their first query.
    named_models: RwLock<HashMap<String, Arc<dyn Embedder>>>,
    /// Writes may store rows without the vector of an embedded field whose
    /// model is not open here ([`Db::set_embed_later`]).
    embed_later: std::sync::atomic::AtomicBool,
    /// Models of derived TEXT fields ([`Db::set_deriver_in`]).
    derivers: RwLock<Vec<Deriving>>,
    /// The dictionaries among them ([`crate::content::DictEntities`]), by
    /// table and field, given their values before each write derives.
    dicts: RwLock<Vec<(String, String, Arc<crate::content::DictEntities>)>>,
    /// The classifiers among them ([`crate::content::ClassifierTags`]),
    /// by table and field.
    classifiers: RwLock<Vec<(String, String, Arc<crate::content::ClassifierTags>)>>,
    /// Where [`Db::open_embedders`] found models, for the tables created
    /// after it, and where ranking models are ([`Db::open_ranking_models`]).
    models: RwLock<Option<PathBuf>>,
    /// The ranking models queries have used.
    ranking: Models,
    /// How writes resolve MEDIA values ([`Db::set_media_resolver`]).
    media: RwLock<Arc<Resolver>>,
    /// Writes waiting for their turn, which share commits ([`Db::write_ops`]).
    writes: writes::Writes,
    /// The stored functions last read from `_functions`, by the stamp of
    /// the table's snapshot they were read from ([`Db::functions_at`]).
    functions: std::sync::Mutex<Option<(u64, Arc<FunctionSet>)>>,
    /// Held while a `CREATE FUNCTION` or `DROP FUNCTION` reads the
    /// versions and writes the next one.
    function_ddl: std::sync::Mutex<()>,
    /// The gazetteers of keyword fields query understanding read
    /// ([`crate::query::understand`]), by table, field and analyzer, with
    /// the stamp of the table's snapshot they were read from and when.
    gazetteers: std::sync::Mutex<HashMap<crate::query::understand::Key, ReadGazetteer>>,
    /// A lock per gazetteer being read, so that one caller reads it while
    /// the others wait for it, or use the one before ([`Db::gazetteers_at`]).
    gazetteer_reads: std::sync::Mutex<HashMap<crate::query::understand::Key, Arc<std::sync::Mutex<()>>>>,
}

/// A gazetteer [`Db::gazetteers_at`] read: the stamp of its table's
/// snapshot, and when.
struct ReadGazetteer {
    stamp: u64,
    at: std::time::Instant,
    g: Arc<crate::query::understand::Gazetteer>,
}

/// How long a query's understanding may use a gazetteer read before its
/// table's last writes ([`Db::gazetteers_at`]): a table written 20 times
/// a second is not read again for each query (audit 4, X-C1). A derived
/// field's dictionary is always the one of its table as it is.
pub const GAZETTEER_FRESH: std::time::Duration = std::time::Duration::from_secs(1);

/// The model of an embedded VECTOR field, and the field it was set for: it
/// serves a field of that name, in a table of that name, with the same
/// embedding and dimensions (so a table dropped and created again with
/// another embedding does not get it).
struct Model {
    table: String,
    field: String,
    embed: Embed,
    dim: usize,
    model: Arc<dyn Embedder>,
}

/// The model of a derived TEXT field, and the field it was set for (as
/// [`Model`] is).
struct Deriving {
    table: String,
    field: String,
    derive: Derive,
    model: Arc<dyn Deriver>,
}

impl Db {
    /// A new database of one table. Its semantic-ID fields' codebooks
    /// come from the default models directory ([`Db::create_in`]).
    pub fn create(path: &Path, schema: Schema, opts: DbOptions) -> Result<Db> {
        Db::create_in(path, schema, opts, &crate::embed::models_dir())
    }

    /// [`Db::create`], with the codebooks of semantic-ID fields from
    /// `models`: the table records each one's hash ([`crate::sid::resolve`]),
    /// and writes get codes from it.
    pub fn create_in(path: &Path, mut schema: Schema, opts: DbOptions, models: &Path) -> Result<Db> {
        crate::sid::resolve(&mut schema, models)?;
        crate::embed::stamp_fingerprints(&mut schema, models);
        let db = Db::wrap(Database::create(path, schema, with_source(opts))?);
        db.open_codebooks(models);
        db.open_content_models();
        Ok(db)
    }

    /// A new database with no tables of its own: its first table is the
    /// placeholder ([`Schema::placeholder`]), and `CREATE TABLE`
    /// ([`Db::create_table`]) adds the tables it holds.
    pub fn create_empty(path: &Path, opts: DbOptions) -> Result<Db> {
        Db::create(path, Schema::placeholder(), opts)
    }

    /// The table a write or a read that names no table goes to: the first
    /// table, or in a database made with no tables
    /// ([`Db::create_empty`]) the first one created after its
    /// placeholder; `None` while it has none.
    pub fn default_table(&self) -> Option<Arc<Schema>> {
        default_table(&self.catalog())
    }

    /// Opens a database; the codebooks of its semantic-ID fields come from
    /// the default models directory, or from [`Db::open_embedders`]'s.
    pub fn open(path: &Path, opts: DbOptions) -> Result<Db> {
        let db = Db::wrap(Database::open(path, with_source(opts))?);
        db.open_codebooks(&crate::embed::models_dir());
        db.open_content_models();
        db.refresh_feeds();
        Ok(db)
    }

    fn wrap(storage: Database) -> Db {
        Db {
            storage: Arc::new(storage),
            readers: RwLock::new(HashMap::new()),
            readers_gen: std::sync::atomic::AtomicU64::new(0),
            exec: ExecOptions {
                filter_cache: Some(Arc::new(crate::query::exec::FilterCache::new(crate::query::exec::FILTER_CACHE_BYTES))),
                memory: crate::query::memory::process_budget(),
                ..ExecOptions::default()
            },
            embedders: RwLock::new(Vec::new()),
            named_models: RwLock::new(HashMap::new()),
            embed_later: std::sync::atomic::AtomicBool::new(false),
            derivers: RwLock::new(Vec::new()),
            dicts: RwLock::new(Vec::new()),
            classifiers: RwLock::new(Vec::new()),
            models: RwLock::new(None),
            media: RwLock::new(Arc::new(Resolver::default())),
            ranking: Models::default(),
            writes: writes::Writes::default(),
            functions: std::sync::Mutex::new(None),
            function_ddl: std::sync::Mutex::new(()),
            gazetteers: std::sync::Mutex::new(HashMap::new()),
            gazetteer_reads: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn storage(&self) -> &Database {
        &self.storage
    }

    /// Flushes in the background from now on ([`Database::spawn_flusher`]).
    pub fn spawn_flusher(&self) {
        self.storage.spawn_flusher();
    }

    /// Merges in the background from now on ([`Database::spawn_merger`]).
    pub fn spawn_merger(&self, opts: crate::storage::MergerOptions) {
        self.storage.spawn_merger(opts);
    }

    /// Makes segments cold and evicts the cold tier's cache in the
    /// background from now on ([`Database::spawn_cold`]); without a cold
    /// tier, does nothing.
    pub fn spawn_cold(&self, every: std::time::Duration) {
        self.storage.spawn_cold(every);
    }

    /// Readies the database for queries once its tables are done changing
    /// (`optimize` in Python, JavaScript and Java): writes the rows in
    /// memory into segments, merges each table's segments into as few as
    /// [`MergePolicy::compact`] allows, without their deleted rows, and
    /// returns the space this frees to the file system
    /// ([`Database::vacuum`]).
    pub fn optimize(&self) -> Result<MergeStats> {
        self.storage.flush()?;
        let stats = self.storage.merge(&MergePolicy::compact())?;
        self.storage.vacuum()?;
        Ok(stats)
    }

    /// What a program that embeds the database runs before it lets go of it
    /// (`close` in Python, JavaScript and Java): stops the background
    /// merger ([`Database::stop_merger`], waiting for a merge it runs),
    /// flushes the rows still in memory into the file
    /// ([`Database::flush`]), which leaves the write-ahead log empty, so
    /// that the file alone holds the database once this returns (a copy of
    /// it is a backup), merges small segments as the merger would
    /// ([`MergePolicy::default`]), then, when an eighth of the file or more
    /// is free, returns that space to the file system
    /// ([`Database::vacuum`]). The free space is what merges freed: later
    /// writes would reuse it, but a program that writes for a while and
    /// stops would leave it in the file. The database still answers and
    /// takes writes after it, without merging in the background. Returns
    /// the bytes the file shrank by. A database opened read-only
    /// ([`DbOptions::read_only`]) is left as it is.
    pub fn close(&self) -> Result<u64> {
        if self.storage.is_read_only() {
            return Ok(0);
        }
        self.storage.stop_merger();
        self.storage.flush()?;
        self.storage.merge(&MergePolicy::default())?;
        let st = self.storage.file_stats();
        // A database of the directory layout has no pages: its vacuum
        // removes only files a crash left behind.
        if st.page_count > 0 && (st.free_pages + st.pending_pages) * 8 < st.page_count {
            return Ok(0);
        }
        self.storage.vacuum()
    }

    /// The schema of the database's first table, which the methods here
    /// that name no table write.
    pub fn schema(&self) -> Arc<Schema> {
        self.storage.schema()
    }

    /// The schemas of the database's tables, the first one first
    /// ([`Database::tables`]), the placeholder of a database made with no
    /// tables among them ([`Db::create_empty`]).
    pub fn catalog(&self) -> Vec<Arc<Schema>> {
        self.storage.tables()
    }

    /// The stream sources written from ([`Database::ensure_sources`]), by
    /// name, with the offset of the last event each wrote; none before
    /// the first.
    pub fn sources(&self) -> Result<Vec<Source>> {
        self.sources_at(&self.snapshot())
    }

    /// The stored functions ([`crate::query::function`]): every version
    /// of each, as `_functions` holds them; none before the first `CREATE
    /// FUNCTION`.
    pub fn functions(&self) -> Result<Arc<FunctionSet>> {
        self.functions_at(&self.snapshot())
    }

    /// [`Db::functions`] as of version `snap`. They are read once per
    /// version of `_functions`, and kept while the table is unchanged.
    pub fn functions_at(&self, snap: &Snapshot) -> Result<Arc<FunctionSet>> {
        let Some(t) = snap.table(FUNCTIONS) else { return Ok(Arc::default()) };
        let stamp = t.stamp();
        if let Some((s, set)) = &*self.functions.lock().unwrap_or_else(|e| e.into_inner()) {
            if *s == stamp {
                return Ok(set.clone());
            }
        }
        // Read as a plain query, which calls no function.
        let sql = format!("SELECT * FROM {FUNCTIONS} LIMIT {}", u32::MAX);
        let q = qsql::parse_query_cached(&sql)?;
        let plan = plan_of_parsed(snap, &q, &Params::new(), &FunctionSet::default())?;
        let rs = self.run_on(snap, &plan, &ExecOptions { max_rows: None, ..self.exec.clone() })?;
        let rows = rs.rows.into_iter().map(|r| function::Function::from_row(&rs.columns.iter().cloned().zip(r).collect()));
        let set = Arc::new(FunctionSet::of(rows.collect::<Result<Vec<_>>>()?));
        *self.functions.lock().unwrap_or_else(|e| e.into_inner()) = Some((stamp, set.clone()));
        Ok(set)
    }

    /// Runs `CREATE FUNCTION` or `DROP FUNCTION` ([`crate::query::function`]),
    /// the version it writes recorded as written by `by` (a server's key
    /// name), and returns what it changed: the version written, the key of
    /// the version deleted, or nothing (the same definition again, or `IF
    /// EXISTS` of a function that is not there).
    pub fn define_function(&self, stmt: &function::Statement, by: Option<&str>) -> Result<function::Change> {
        let _ddl = self.function_ddl.lock().unwrap_or_else(|e| e.into_inner());
        let set = self.functions()?;
        let change = function::change(&set, stmt, Some(crate::timestamp::now()), by)?;
        match &change {
            function::Change::Nothing => {}
            function::Change::Put(f) => {
                if let function::Statement::Create { def, at: None, .. } = stmt {
                    self.check_function(def)?;
                }
                self.storage.ensure_functions()?;
                let mut t = self.storage.begin()?;
                t.table(FUNCTIONS)?.upsert(f.to_row())?;
                t.commit()?;
            }
            function::Change::Delete(id) => {
                let mut t = self.storage.begin()?;
                t.table(FUNCTIONS)?.delete(&Value::Str(id.clone()))?;
                t.commit()?;
            }
        }
        if !matches!(change, function::Change::Nothing) {
            self.refresh_feeds();
        }
        Ok(change)
    }

    /// Refuses a table function whose body does not plan against this
    /// database's tables, each parameter given its default or a value of
    /// its type.
    fn check_function(&self, def: &function::Definition) -> Result<()> {
        if def.kind != function::Kind::Table {
            return Ok(());
        }
        // A vector of no stated size: the body is checked when called.
        let Some(params) = function::sample_params(def) else { return Ok(()) };
        let snap = self.snapshot();
        let q = qsql::parse_query(&def.body)?;
        plan_of(&snap, &q, &params, &*self.functions_at(&snap)?).map(drop).map_err(|e| Error::Query(format!("the body of {} does not plan: {e}", def.name)))
    }

    /// [`Db::sources`] as of version `snap`.
    pub fn sources_at(&self, snap: &Snapshot) -> Result<Vec<Source>> {
        if snap.table(SOURCES).is_none() {
            return Ok(Vec::new());
        }
        let sql = format!("SELECT name, last_offset, updated FROM {SOURCES} LIMIT {}", u32::MAX);
        let rs = self.query_at(snap, &sql, &Params::new(), &ExecOptions { max_rows: None, ..self.exec.clone() })?;
        let mut sources: Vec<Source> = rs
            .rows
            .into_iter()
            .filter_map(|r| {
                let mut r = r.into_iter();
                let name = match r.next()? {
                    Value::Str(s) => s,
                    _ => return None,
                };
                let offset = r.next()?.as_i64()?;
                let updated = r.next().and_then(|v| match v {
                    Value::Timestamp(t) | Value::Int(t) => Some(t),
                    _ => None,
                });
                Some(Source { name, offset, updated })
            })
            .collect();
        sources.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sources)
    }

    /// The schema of the table named `table` (any case).
    fn table(&self, table: &str) -> Result<Arc<Schema>> {
        self.storage.table(table).ok_or_else(|| Error::Schema(format!("no table {table}")))
    }

    /// Adds an empty table ([`Database::create_table`], SQL `CREATE
    /// TABLE`). SQL reads and writes it by name, and so do
    /// [`crate::storage::WriteTxn::table`] and [`Snapshot::table`]. After
    /// [`Db::open_embedders`], its embedded fields get their models from the
    /// same place (a model it cannot open leaves the vectors to the writes).
    pub fn create_table(&self, mut schema: Schema) -> Result<()> {
        let name = schema.table.clone();
        for f in &schema.fields {
            no_self_dictionary(&name, f)?;
        }
        let dir = self.models.read().unwrap_or_else(|e| e.into_inner()).clone();
        let models = dir.clone().unwrap_or_else(crate::embed::models_dir);
        crate::sid::resolve(&mut schema, &models)?;
        crate::embed::stamp_fingerprints(&mut schema, &models);
        self.storage.create_table(schema)?;
        if let Some(dir) = dir {
            self.open_embedders_of(&name, &dir);
        }
        self.open_codebooks_of(&name, &models);
        self.open_content_models_of(&name);
        Ok(())
    }

    /// Changes the columns of the table named `table`
    /// ([`Database::alter_table`], SQL `ALTER TABLE`). The models of its
    /// embedded and derived columns follow them: a renamed column keeps
    /// its, and so does one whose sources are renamed; an added one gets
    /// its as a new table's columns do, after [`Db::open_embedders`].
    pub fn alter_table(&self, table: &str, alters: &[Alter]) -> Result<()> {
        let before = self.table(table)?;
        for a in alters {
            if let Alter::AddField(f) = a {
                no_self_dictionary(&before.table, f)?;
            }
        }
        self.storage.alter_table(table, alters)?;
        let after = self.table(table)?;
        self.columns_changed(&before, &after);
        Ok(())
    }

    /// Makes the models of a table follow its columns, which `ALTER TABLE`
    /// changed from `before` to `after` (here, or on the primary that
    /// shipped the change): a renamed column keeps its model, and so does
    /// one whose sources are renamed; a dropped one loses it; an added one,
    /// or one whose model is another now, gets its own after
    /// [`Db::open_embedders`].
    fn columns_changed(&self, before: &Schema, after: &Schema) {
        // A model serves a field by its table's and its own name, and the
        // spec it was given; the field it served, by id, now.
        let now = |field: &str| before.field(field).and_then(|b| after.fields.iter().find(|a| a.id == b.id).map(|a| (b, a)));
        let ours = |t: &str| t.eq_ignore_ascii_case(&before.table);
        self.embedders.write().unwrap_or_else(|e| e.into_inner()).retain_mut(|m| {
            if !ours(&m.table) {
                return true;
            }
            let Some((b, a)) = now(&m.field) else { return false };
            if let (Some(was), Some(is)) = (&b.embed, &a.embed) {
                if *was == m.embed && was.model == is.model {
                    m.embed = is.clone();
                }
            }
            m.field = a.name.clone();
            true
        });
        self.derivers.write().unwrap_or_else(|e| e.into_inner()).retain_mut(|m| {
            if !ours(&m.table) {
                return true;
            }
            let Some((b, a)) = now(&m.field) else { return false };
            if let (Some(was), Some(is)) = (&b.derive, &a.derive) {
                if *was == m.derive && was.model == is.model {
                    m.derive = is.clone();
                }
            }
            m.field = a.name.clone();
            true
        });
        let dir = self.models.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(dir) = dir {
            let model = |f: &crate::schema::Field| (f.embed.as_ref().map(|e| e.model.clone()), f.derive.as_ref().map(|d| d.model.clone()));
            let new = |f: &crate::schema::Field| before.fields.iter().find(|b| b.id == f.id).is_none_or(|b| model(b) != model(f));
            self.open_models_of(&after.table, &dir, new);
        }
        self.open_content_models_of(&after.table);
    }

    /// Drops a table other than the first ([`Database::drop_table`]).
    pub fn drop_table(&self, name: &str) -> Result<()> {
        self.drop_tables(&[name], DropOptions::default())
    }

    /// Drops tables ([`Database::drop_tables`], SQL `DROP TABLE`).
    pub fn drop_tables(&self, names: &[&str], opts: DropOptions) -> Result<()> {
        let r = self.storage.drop_tables(names, opts);
        let tables = self.catalog();
        self.embedders.write().unwrap_or_else(|e| e.into_inner()).retain(|m| tables.iter().any(|t| t.table.eq_ignore_ascii_case(&m.table)));
        r
    }

    /// Applies transactions a primary shipped ([`Database::apply_shipped`]).
    /// A table they create gets its models as [`Db::create_table`] gives
    /// them, and a table they drop loses its own.
    pub fn apply_shipped(&self, prev: SeqNo, txns: &[&[u8]]) -> Result<Applied> {
        let before = self.catalog();
        let r = self.storage.apply_shipped(prev, txns);
        self.tables_changed(&before);
        self.refresh_feeds();
        r
    }

    /// Puts a copy of its primary in place of this database's rows and
    /// tables ([`Database::install_copy`]); the tables that come with it get
    /// their models as [`Db::apply_shipped`] gives them.
    pub fn install_copy(&self, copy: &Database) -> Result<SeqNo> {
        let before = self.catalog();
        let r = self.storage.install_copy(copy);
        self.tables_changed(&before);
        self.refresh_feeds();
        r
    }

    /// [`Db::install_copy`] by [`Database::install_copy_over`].
    pub fn install_copy_over(&self, copy: &Database) -> Result<SeqNo> {
        let before = self.catalog();
        let r = self.storage.install_copy_over(copy);
        self.tables_changed(&before);
        r
    }

    /// Opens the models of the tables made since the catalog was `before`,
    /// as [`Db::create_table`] does, and forgets those of the tables gone.
    /// Those of a table whose columns changed follow them, as they do on
    /// the primary that altered it ([`Db::alter_table`]).
    fn tables_changed(&self, before: &[Arc<Schema>]) {
        let now = self.catalog();
        if now.len() == before.len() && now.iter().zip(before).all(|(a, b)| a == b) {
            return;
        }
        self.embedders.write().unwrap_or_else(|e| e.into_inner()).retain(|m| now.iter().any(|t| t.table.eq_ignore_ascii_case(&m.table)));
        let dir = self.models.read().unwrap_or_else(|e| e.into_inner()).clone();
        let models = dir.clone().unwrap_or_else(crate::embed::models_dir);
        for t in now.iter().filter(|t| !before.contains(t)) {
            match before.iter().find(|b| b.table.eq_ignore_ascii_case(&t.table)).filter(|b| t.follows(b)) {
                Some(b) => self.columns_changed(b, t),
                None => {
                    if let Some(dir) = &dir {
                        self.open_embedders_of(&t.table, dir);
                    }
                }
            }
            // By the place of their field, which a copy may have moved.
            self.open_codebooks_of(&t.table, &models);
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.storage.snapshot()
    }

    /// Bulk-loads documents into the first table (generated columns are
    /// filled in, and embedded vectors the documents leave out are
    /// computed).
    pub fn load(&self, docs: impl IntoIterator<Item = Document>) -> Result<()> {
        self.try_load(docs.into_iter().map(Ok))
    }

    /// [`Db::load`] of documents that may fail to arrive (rows read from a
    /// file): the first error ends the load, and nothing is committed.
    pub fn try_load(&self, docs: impl IntoIterator<Item = Result<Document>>) -> Result<()> {
        let first = self.schema().table.clone();
        self.try_load_into(&first, docs)
    }

    /// [`Db::load`] into the table named `table`.
    pub fn load_into(&self, table: &str, docs: impl IntoIterator<Item = Document>) -> Result<()> {
        self.try_load_into(table, docs.into_iter().map(Ok))
    }

    /// [`Db::try_load`] into the table named `table`.
    pub fn try_load_into(&self, table: &str, docs: impl IntoIterator<Item = Result<Document>>) -> Result<()> {
        let schema = self.table(table)?;
        let models = self.write_models_of(&schema);
        let derivers = self.derivers_of(&schema);
        let media = self.media_resolver();
        let mut docs = docs.into_iter();
        if models.iter().all(Option::is_none) && derivers.iter().all(Option::is_none) && !schema.fields.iter().any(|f| f.media.is_some()) {
            return self.storage.try_bulk_load_into(table, docs.map(move |d| {
                d.map(|mut d| {
                    schema.fill_generated(&mut d);
                    d
                })
            }));
        }
        // Resolved and embedded in batches, so that fetches run together
        // and a model sees many texts at once; fewer rows of images, whose
        // bytes are held until a model has read them.
        let batch_size = if (0..schema.fields.len()).any(|f| read_by_model(&schema, f)) { MEDIA_BATCH } else { EMBED_BATCH };
        let mut ready: std::vec::IntoIter<Result<Document>> = Vec::new().into_iter();
        self.storage.try_bulk_load_into(
            table,
            std::iter::from_fn(|| loop {
                if let Some(d) = ready.next() {
                    return Some(d);
                }
                let mut batch = Vec::new();
                for d in docs.by_ref().take(batch_size) {
                    match d {
                        Ok(d) => batch.push(d),
                        Err(e) => {
                            // The rows before the error are not loaded either.
                            return Some(Err(e));
                        }
                    }
                }
                if batch.is_empty() {
                    return None;
                }
                batch.iter_mut().for_each(|d| schema.fill_generated(d));
                let embedded = resolve_media(&media, &schema, &mut batch).and_then(|held| {
                    let files = Files { resolver: &media, held };
                    derive_known(&derivers, &schema, &mut batch, &Derived::new(), &files)?;
                    embed_with(&models, &schema, &mut batch, &files)
                });
                if let Err(e) = embedded {
                    return Some(Err(e));
                }
                ready = batch.into_iter().map(Ok).collect::<Vec<_>>().into_iter();
            }),
        )
    }

    /// Every live document of `snap` as merges rebuild them (stored fields,
    /// vectors, values written in place, generated text), segment by
    /// segment. Fields neither stored nor recoverable from an index are null.
    pub fn documents<'a>(&self, snap: &'a Snapshot) -> impl Iterator<Item = Result<Document>> + 'a {
        let (mut seg, mut next) = (0usize, 0 as DocId);
        let mut ready = Vec::new().into_iter();
        std::iter::from_fn(move || loop {
            if let Some(d) = ready.next() {
                return Some(Ok(d));
            }
            let v = snap.segments().get(seg)?;
            let n = v.file.n_docs() as DocId;
            if next >= n {
                (seg, next) = (seg + 1, 0);
                continue;
            }
            let end = n.min(next.saturating_add(4096));
            let live: Vec<DocId> = (next..end).filter(|&d| v.live.as_ref().is_none_or(|l| l.contains(d))).collect();
            next = end;
            match StoredSource.documents(snap.schema(), &v.file, &live) {
                Ok(mut docs) => {
                    if let Some(u) = &v.updates {
                        for (doc, &d) in docs.iter_mut().zip(&live) {
                            u.apply(d, doc);
                        }
                    }
                    ready = docs.into_iter();
                }
                Err(e) => {
                    seg = usize::MAX;
                    return Some(Err(e));
                }
            }
        })
    }

    /// Rows `rows` of the table `snap` looks at, each a segment ordinal
    /// and a docID, increasing within each segment ([`Snapshot::covered`]
    /// gives them so), rebuilt as [`Db::documents`] rebuilds every row.
    pub fn documents_of(&self, snap: &Snapshot, rows: &[(usize, DocId)]) -> Result<Vec<Document>> {
        let mut out = Vec::with_capacity(rows.len());
        for group in rows.chunk_by(|a, b| a.0 == b.0) {
            let v = snap.segments().get(group[0].0).ok_or_else(|| Error::Query(format!("segment {} of {}", group[0].0, snap.segments().len())))?;
            let docs: Vec<DocId> = group.iter().map(|r| r.1).collect();
            let mut got = StoredSource.documents(snap.schema(), &v.file, &docs)?;
            if let Some(u) = &v.updates {
                for (doc, &d) in got.iter_mut().zip(&docs) {
                    u.apply(d, doc);
                }
            }
            out.extend(got);
        }
        Ok(out)
    }

    /// Bulk-loads the rows of `other`, a database with another schema,
    /// matching fields by name: to add, drop or re-embed fields, create a
    /// database with the new schema and load the old one into it. A VECTOR
    /// field is copied when the old field has the same dimensions and
    /// embedding ([`crate::schema::Embed`]); otherwise this database embeds
    /// it anew (given the model, [`Db::set_embedder`]). A semantic-ID field
    /// is copied when the old field has the same codebook, and derived anew
    /// otherwise: a new codebook means a new field, filled this way. So is
    /// a derived TEXT field, by its model ([`crate::schema::Derive`]).
    /// Returns the rows loaded.
    pub fn load_from(&self, other: &Db) -> Result<u64> {
        self.load_table_from(&self.schema().table, other, &other.schema().table)
    }

    /// [`Db::load_from`] of the rows of table `from` of `other` into this
    /// database's table `table`.
    pub fn load_table_from(&self, table: &str, other: &Db, from: &str) -> Result<u64> {
        let (old, new) = (&*other.table(from)?, &*self.table(table)?);
        if let Some(f) = old.unrebuildable() {
            return Err(crate::Error::Schema(format!("{} is not stored, so its values cannot be copied", f.name)));
        }
        let map: Vec<Option<usize>> = new
            .fields
            .iter()
            .map(|f| {
                let j = old.field_index(&f.name)?;
                let g = &old.fields[j];
                let keep = match (&f.ty, &g.ty) {
                    (FieldType::Vector { dim: a, .. }, FieldType::Vector { dim: b, .. }) => a == b && f.embed == g.embed,
                    // Codes of another codebook are derived anew.
                    _ if f.semantic_id.is_some() || g.semantic_id.is_some() => f.semantic_id == g.semantic_id,
                    // Text of another model is made anew.
                    _ if f.derive.is_some() || g.derive.is_some() => f.derive == g.derive,
                    (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b) && f.multi == g.multi && f.media == g.media,
                };
                keep.then_some(j)
            })
            .collect();
        let snap = other.snapshot().table(from).ok_or_else(|| Error::Query(format!("no such table: {from}")))?;
        let mut failed = None;
        let mut n = 0u64;
        let docs = other.documents(&snap).map_while(|d| match d {
            Ok(d) => {
                n += 1;
                Some(map.iter().map(|j| j.map_or(Value::Null, |j| d[j].clone())).collect())
            }
            Err(e) => {
                failed = Some(e);
                None
            }
        });
        self.load_into(table, docs)?;
        match failed {
            Some(e) => Err(e),
            None => Ok(n),
        }
    }

    /// Gives embedded VECTOR or SPARSE field `field`
    /// ([`crate::schema::Embed`]) of the first table its model. Writes through this `Db` that leave the vector
    /// out get it computed ([`Db::load`], SQL `INSERT` and `UPDATE`,
    /// [`Db::embed_missing`] before a [`crate::storage::WriteTxn`]).
    pub fn set_embedder(&self, field: &str, e: Arc<dyn Embedder>) -> Result<()> {
        let first = self.schema().table.clone();
        self.set_embedder_in(&first, field, e)
    }

    /// [`Db::set_embedder`] for a field of the table named `table`.
    pub fn set_embedder_in(&self, table: &str, field: &str, e: Arc<dyn Embedder>) -> Result<()> {
        let schema = self.table(table)?;
        let i = schema.field_index(field).ok_or_else(|| Error::Schema(format!("no field {field}")))?;
        let f = &schema.fields[i];
        if let (FieldType::Sparse, Some(spec)) = (&f.ty, &f.embed) {
            if !e.sparse() {
                return Err(Error::Schema(format!("model {} makes vectors, and field {field} is a SPARSE field: it needs a learned sparse model, which makes term weights", spec.model)));
            }
            let mut all = self.embedders.write().unwrap_or_else(|e| e.into_inner());
            all.retain(|m| !(m.table.eq_ignore_ascii_case(&schema.table) && m.field.eq_ignore_ascii_case(&f.name)));
            all.push(Model { table: schema.table.clone(), field: f.name.clone(), embed: spec.clone(), dim: 0, model: e });
            return Ok(());
        }
        let (FieldType::Vector { dim, .. }, Some(spec)) = (&f.ty, &f.embed) else {
            return Err(Error::Schema(format!("field {field} is not an embedded VECTOR or SPARSE field")));
        };
        if e.sparse() {
            return Err(Error::Schema(format!("model {} is a learned sparse model, which makes term weights: field {field} is a VECTOR field, and a SPARSE field holds its output", spec.model)));
        }
        if e.dim() != *dim {
            return Err(Error::Schema(format!("model {} makes {}-dimension vectors; field {field} holds {dim}", spec.model, e.dim())));
        }
        if f.multi && !e.tokens() {
            return Err(Error::Schema(format!("model {} embeds a text into one vector, and field {field} is a multi-vector field: it needs a model of a vector a token, such as ColBERT (answerai-colbert-small-v1)", spec.model)));
        }
        if !f.multi && !e.single() {
            return Err(Error::Schema(format!("model {} embeds a text into a vector a token: field {field} needs to be a multi-vector field, VECTOR({dim})[]", spec.model)));
        }
        if let Some(m) = schema.embeds_media(i) {
            let (wants, what) = match schema.fields[m].media {
                Some(crate::media::MediaKind::Audio) => (Modality::Audio, "sound"),
                _ => (Modality::Image, "images"),
            };
            if !e.modalities().contains(&wants) {
                return Err(Error::Schema(format!("model {} does not embed {what}, and field {field} is the embedding of the {what} of {}", spec.model, schema.fields[m].name)));
            }
        }
        let mut all = self.embedders.write().unwrap_or_else(|e| e.into_inner());
        all.retain(|m| !(m.table.eq_ignore_ascii_case(&schema.table) && m.field.eq_ignore_ascii_case(&f.name)));
        all.push(Model { table: schema.table.clone(), field: f.name.clone(), embed: spec.clone(), dim: *dim, model: e });
        Ok(())
    }

    /// The parameter a query gives for text `text` against embedded field
    /// `field` of table `table`: the vector of the text as a query by the
    /// field's model, as `similarity` takes it, or for a multi-vector field
    /// (`VECTOR(d)[]`) the list of the vectors of its tokens
    /// ([`Embedder::embed_query_tokens`]), as `maxsim` takes them, or for
    /// a SPARSE field the query's term weights
    /// ([`Embedder::embed_query_sparse`]), as `similarity` takes them.
    pub fn embed_query_in(&self, table: &str, field: &str, text: &str) -> Result<Value> {
        let schema = self.table(table)?;
        let f = schema.field(field).ok_or_else(|| Error::Schema(format!("no field {field}")))?;
        let e = self.embedder_in(table, field).ok_or_else(|| Error::Schema(format!("field {field} has no model here (Db::open_embedders or Db::set_embedder_in)")))?;
        Ok(match f.multi {
            _ if f.ty == FieldType::Sparse => Value::Str(crate::embed::terms_json(&e.embed_query_sparse(text)?)),
            true => Value::List(e.embed_query_tokens(text)?.into_iter().map(|v| Value::Vector(v.into())).collect()),
            false => Value::Vector(e.embed_query(text)?.into()),
        })
    }

    /// The model of embedded field `field` of the first table, if it has one.
    pub fn embedder(&self, field: &str) -> Option<Arc<dyn Embedder>> {
        self.embedder_in(&self.schema().table, field)
    }

    /// The model of embedded field `field` of the table named `table`.
    pub fn embedder_in(&self, table: &str, field: &str) -> Option<Arc<dyn Embedder>> {
        let schema = self.storage.table(table)?;
        let i = schema.field_index(field)?;
        self.models_of(&schema).swap_remove(i)
    }

    /// Lets writes store rows without the vector of an embedded field whose
    /// model is not open here (off by default: such a write fails, since
    /// `similarity()` would never find the row). [`Db::embed_backfill`]
    /// fills the vectors in once the model is open.
    pub fn set_embed_later(&self, on: bool) {
        self.embed_later.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// The models a write embeds the fields of `schema` with: those open
    /// ([`Db::models_of`]), and for a field of a model this database can
    /// run but has not open, one that fails the write with what to do,
    /// unless [`Db::set_embed_later`] lets the rows go without vectors.
    fn write_models_of(&self, schema: &Schema) -> Vec<Option<Arc<dyn Embedder>>> {
        let mut models = self.models_of(schema);
        if self.embed_later.load(std::sync::atomic::Ordering::Relaxed) {
            return models;
        }
        for (m, f) in models.iter_mut().zip(&schema.fields) {
            if let (None, Some(e), FieldType::Vector { dim, .. }) = (&m, &f.embed, &f.ty) {
                if crate::embed::known_dim(&e.model).is_some() {
                    *m = Some(Arc::new(crate::embed::NotOpen { table: schema.table.clone(), field: f.name.clone(), model: e.model.clone(), dim: *dim }));
                }
            }
        }
        models
    }

    /// The models of the embedded fields of the table `schema` describes,
    /// by field.
    fn models_of(&self, schema: &Schema) -> Vec<Option<Arc<dyn Embedder>>> {
        let all = self.embedders.read().unwrap_or_else(|e| e.into_inner());
        schema
            .fields
            .iter()
            .map(|f| {
                let serves = |m: &&Model| {
                    let fits = match f.ty {
                        FieldType::Vector { dim, .. } => dim == m.dim && !m.model.sparse(),
                        FieldType::Sparse => m.model.sparse(),
                        _ => false,
                    };
                    m.table.eq_ignore_ascii_case(&schema.table) && m.field.eq_ignore_ascii_case(&f.name) && f.embed.as_ref() == Some(&m.embed) && fits
                };
                all.iter().find(serves).map(|m| m.model.clone())
            })
            .collect()
    }

    /// Gives derived TEXT field `field` ([`crate::schema::Derive`]) of the
    /// table named `table` its model. Writes through this `Db` that leave
    /// the text out get it made, as they get embedded vectors.
    pub fn set_deriver_in(&self, table: &str, field: &str, d: Arc<dyn Deriver>) -> Result<()> {
        let schema = self.table(table)?;
        let f = schema.field(field).ok_or_else(|| Error::Schema(format!("no field {field}")))?;
        let Some(spec) = &f.derive else { return Err(Error::Schema(format!("field {field} is not a derived TEXT field"))) };
        let Some(kind) = schema.field(&spec.from).and_then(|m| m.media) else {
            // Text of text: a language model's.
            if d.modality() != Modality::Text {
                return Err(Error::Schema(format!("model {} makes text of {}s, and {} is not a MEDIA field", spec.model, d.modality().as_str(), spec.from)));
            }
            let mut all = self.derivers.write().unwrap_or_else(|e| e.into_inner());
            all.retain(|m| !(m.table.eq_ignore_ascii_case(&schema.table) && m.field.eq_ignore_ascii_case(&f.name)));
            all.push(Deriving { table: schema.table.clone(), field: f.name.clone(), derive: spec.clone(), model: d });
            return Ok(());
        };
        let wants = match d.modality() {
            Modality::Image => crate::media::MediaKind::Image,
            Modality::Audio => crate::media::MediaKind::Audio,
            Modality::Text => return Err(Error::Schema(format!("model {} makes text of text, not of media", spec.model))),
        };
        if kind != wants {
            return Err(Error::Schema(format!(
                "model {} makes text of {}, and {} is a MEDIA field of kind {}: declare kind = '{}'",
                spec.model,
                if wants == crate::media::MediaKind::Image { "images" } else { "audio" },
                spec.from,
                kind.as_str(),
                wants.as_str()
            )));
        }
        let mut all = self.derivers.write().unwrap_or_else(|e| e.into_inner());
        all.retain(|m| !(m.table.eq_ignore_ascii_case(&schema.table) && m.field.eq_ignore_ascii_case(&f.name)));
        all.push(Deriving { table: schema.table.clone(), field: f.name.clone(), derive: spec.clone(), model: d });
        Ok(())
    }

    /// The model of derived field `field` of the table named `table`.
    pub fn deriver_in(&self, table: &str, field: &str) -> Option<Arc<dyn Deriver>> {
        let schema = self.storage.table(table)?;
        let i = schema.field_index(field)?;
        self.derivers_of(&schema).swap_remove(i)
    }

    /// The models of the derived fields of the table `schema` describes, by
    /// field.
    fn derivers_of(&self, schema: &Schema) -> Vec<Option<Arc<dyn Deriver>>> {
        self.refresh_dicts(schema);
        self.refresh_classifiers(schema);
        let all = self.derivers.read().unwrap_or_else(|e| e.into_inner());
        schema
            .fields
            .iter()
            .map(|f| {
                let serves = |m: &&Deriving| m.table.eq_ignore_ascii_case(&schema.table) && m.field.eq_ignore_ascii_case(&f.name) && f.derive.as_ref() == Some(&m.derive);
                all.iter().find(serves).map(|m| m.model.clone())
            })
            .collect()
    }

    /// Gives the dictionaries of the table `schema` describes the values
    /// their tables hold now (read once per version of those tables); a
    /// dictionary whose table or field is gone keeps the values it had.
    fn refresh_dicts(&self, schema: &Schema) {
        let dicts: Vec<Arc<crate::content::DictEntities>> = self
            .dicts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(t, _, _)| t.eq_ignore_ascii_case(&schema.table))
            .map(|(_, _, d)| d.clone())
            .collect();
        if dicts.is_empty() {
            return;
        }
        let snap = self.snapshot();
        for d in dicts {
            let Some(t) = snap.schemas().find(|s| s.table.eq_ignore_ascii_case(&d.table)).map(|s| s.table.clone()) else { continue };
            let need = crate::query::understand::Need::listed((t, d.field.clone(), d.analyzer));
            if let Ok(g) = self.gazetteers_at(&snap, std::slice::from_ref(&need), &self.exec, true) {
                if let Some(g) = g.get(&need.key) {
                    d.set(g.clone());
                }
            }
        }
    }

    /// Gives the classifiers of the derived fields of the table `schema`
    /// describes their current versions (decoded once a version), and the
    /// models of the fields they were trained on.
    fn refresh_classifiers(&self, schema: &Schema) {
        let mine: Vec<Arc<crate::content::ClassifierTags>> = self
            .classifiers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(t, _, _)| t.eq_ignore_ascii_case(&schema.table))
            .map(|(_, _, c)| c.clone())
            .collect();
        if mine.is_empty() {
            return;
        }
        let Ok(functions) = self.functions() else { return };
        for tags in mine {
            match functions.current(&tags.name) {
                Some(f) if tags.version() != Some(f.version) => {
                    if let Ok(Some(c)) = crate::query::classify::of_function(f) {
                        let e = self.embedder_in(&c.table, &c.field);
                        tags.set(f.version, c, e);
                    }
                }
                Some(_) => {}
                None => tags.clear(),
            }
        }
    }

    /// Opens the built-in models and dictionaries of the derived fields of
    /// table `table` ([`crate::content`]), which need no model files.
    fn open_content_models_of(&self, table: &str) {
        let Some(schema) = self.storage.table(table) else { return };
        for f in &schema.fields {
            let Some(d) = &f.derive else { continue };
            let model: Arc<dyn Deriver> = if d.model.starts_with(crate::content::BUILTIN) {
                match crate::content::open(&d.model) {
                    Ok(m) => m,
                    Err(_) => continue,
                }
            } else if d.model.starts_with(crate::content::DICT) {
                let a = schema.field(&d.from).map(|s| s.text_analyzer()).unwrap_or_default();
                let Ok(dict) = crate::content::DictEntities::new(&d.model, a) else { continue };
                let dict = Arc::new(dict);
                let mut all = self.dicts.write().unwrap_or_else(|e| e.into_inner());
                all.retain(|(t, n, _)| !(t.eq_ignore_ascii_case(&schema.table) && n.eq_ignore_ascii_case(&f.name)));
                all.push((schema.table.clone(), f.name.clone(), dict.clone()));
                dict
            } else if d.model.starts_with(crate::content::CLASSIFIER) {
                let Ok(tags) = crate::content::ClassifierTags::new(&d.model, f.multi) else { continue };
                let tags = Arc::new(tags);
                let mut all = self.classifiers.write().unwrap_or_else(|e| e.into_inner());
                all.retain(|(t, n, _)| !(t.eq_ignore_ascii_case(&schema.table) && n.eq_ignore_ascii_case(&f.name)));
                all.push((schema.table.clone(), f.name.clone(), tags.clone()));
                tags
            } else {
                continue;
            };
            let _ = self.set_deriver_in(table, &f.name, model);
        }
    }

    /// [`Db::open_content_models_of`] for every table.
    fn open_content_models(&self) {
        for t in self.catalog() {
            self.open_content_models_of(&t.table);
        }
    }

    /// Opens the model of every embedded field of every table from `models`
    /// ([`crate::embed::open`]), and of every derived field
    /// ([`crate::derive::open`]), and those of the tables created later
    /// ([`Db::create_table`]); returns the fields whose model did not open,
    /// with the reason (named `table.field` outside the first table). Their
    /// vectors or text must come with the writes.
    pub fn open_embedders(&self, models: &Path) -> Vec<(String, Error)> {
        *self.models.write().unwrap_or_else(|e| e.into_inner()) = Some(models.to_path_buf());
        let first = self.schema().table.clone();
        let mut failed = Vec::new();
        for t in self.catalog() {
            for (f, e) in self.open_embedders_of(&t.table, models) {
                failed.push((if t.table == first { f } else { format!("{}.{f}", t.table) }, e));
            }
        }
        failed.extend(self.open_codebooks(models));
        failed
    }

    /// Opens the codebook of every semantic-ID field from `models`
    /// ([`crate::sid::Codebook::open`]), checked against the hash the field
    /// records; returns the fields whose codebook did not open, as
    /// [`Db::open_embedders`] does. Writes that need codes from those fail.
    pub fn open_codebooks(&self, models: &Path) -> Vec<(String, Error)> {
        let first = self.schema().table.clone();
        let mut failed = Vec::new();
        for t in self.catalog() {
            for (f, e) in self.open_codebooks_of(&t.table, models) {
                failed.push((if t.table == first { f } else { format!("{}.{f}", t.table) }, e));
            }
        }
        failed
    }

    fn open_codebooks_of(&self, table: &str, models: &Path) -> Vec<(String, Error)> {
        let Some(schema) = self.storage.table(table) else { return Vec::new() };
        let mut failed = Vec::new();
        for f in &schema.fields {
            let Some(sid) = &f.semantic_id else { continue };
            let opened = crate::sid::Codebook::open(&sid.codebook, models).and_then(|(cb, hash)| self.storage.set_codebook(table, &f.name, cb, &hash));
            if let Err(e) = opened {
                failed.push((f.name.clone(), e));
            }
        }
        failed
    }

    fn open_embedders_of(&self, table: &str, models: &Path) -> Vec<(String, Error)> {
        self.open_models_of(table, models, |_| true)
    }

    /// [`Db::open_embedders_of`], for the fields `which` picks.
    fn open_models_of(&self, table: &str, models: &Path, which: impl Fn(&crate::schema::Field) -> bool) -> Vec<(String, Error)> {
        let Some(schema) = self.storage.table(table) else { return Vec::new() };
        let mut failed = Vec::new();
        for f in schema.fields.iter().filter(|f| which(f)) {
            if f.derive.as_ref().is_some_and(|d| crate::content::is_content_model(&d.model)) {
                continue;
            }
            let opened = match (&f.embed, &f.derive) {
                (Some(spec), _) => crate::embed::check_fingerprint(&f.name, &spec.model, spec.fingerprint.as_deref(), models)
                    .and_then(|_| crate::embed::open_for(&spec.model, match f.ty { FieldType::Vector { dim, .. } => Some(dim), _ => None }, models))
                    .and_then(|m| self.set_embedder_in(table, &f.name, m)),
                (_, Some(spec)) => crate::derive::open_for(spec, models).and_then(|m| self.set_deriver_in(table, &f.name, m)),
                _ => continue,
            };
            if let Err(e) = opened {
                failed.push((f.name.clone(), e));
            }
        }
        failed
    }

    /// Opens the ranking models a plan's `gbdt()` and `cross_encoder()`
    /// calls name ([`crate::query::model`]) from the models directory: the
    /// one [`Db::open_embedders`] was given, else
    /// [`crate::embed::models_dir`]. A `shard`
    /// ([`crate::query::exec::ExecOptions::shard`]) leaves cross-encoders
    /// to its coordinator. A plan that names the hash of another file of a
    /// model is refused; one without gets the hashes of the files opened.
    /// [`Db::run`] and [`Db::query`] open a plan's models themselves.
    pub fn open_ranking_models(&self, plan: &mut Plan, shard: bool) -> Result<()> {
        if !plan.root.has_models() {
            return Ok(());
        }
        let dir = self.models.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(crate::embed::models_dir);
        self.ranking.open(&dir, plan, !shard)
    }

    /// Trains a semantic-ID codebook ([`crate::sid::Codebook::train`]) on the
    /// vectors of VECTOR field `field` of the table named `table`, and saves
    /// it as `name` under `models`, for semantic-ID fields to name. Returns
    /// the SHA-256 of its file.
    pub fn train_codebook(&self, table: &str, field: &str, name: &str, models: &Path, opts: &crate::sid::TrainOptions) -> Result<String> {
        let (vectors, dim) = self.field_vectors(table, field)?;
        if vectors.is_empty() {
            return Err(Error::Schema(format!("{table}.{field} holds no vectors to train a codebook on")));
        }
        crate::sid::Codebook::train(&vectors, dim, opts)?.save(name, models)
    }

    /// What the codebook saved as `name` under `models` makes of the
    /// vectors of VECTOR field `field` of the table named `table`
    /// ([`crate::sid::Codebook::report`]): how many rows share a code and
    /// how evenly each level's numbers are used, before or after a
    /// semantic-ID field names it.
    pub fn codebook_report(&self, table: &str, field: &str, name: &str, models: &Path) -> Result<crate::sid::Report> {
        let (vectors, dim) = self.field_vectors(table, field)?;
        let (cb, _) = crate::sid::Codebook::open(name, models)?;
        if cb.dim != dim {
            return Err(Error::Schema(format!("codebook {name} is of {} dimensions, {table}.{field} of {dim}", cb.dim)));
        }
        if vectors.is_empty() {
            return Err(Error::Schema(format!("{table}.{field} holds no vectors")));
        }
        cb.report(&vectors)
    }

    /// The live rows' vectors of VECTOR field `field` of the table named
    /// `table`, one after another, and their dimension.
    fn field_vectors(&self, table: &str, field: &str) -> Result<(Vec<f32>, usize)> {
        let snap = self.snapshot().table(table).ok_or_else(|| Error::Schema(format!("no table {table}")))?;
        let schema = snap.schema();
        let f = schema.field(field).ok_or_else(|| Error::Schema(format!("no field {field}")))?;
        let FieldType::Vector { dim, .. } = f.ty else { return Err(Error::Schema(format!("field {field} is not a VECTOR field"))) };
        let mut vectors = Vec::new();
        for v in snap.segments() {
            let r = VectorReader::open(&v.file, &f.key())?;
            for d in 0..v.file.n_docs() as DocId {
                if v.live.as_ref().is_none_or(|l| l.contains(d)) && r.has_vector(d) {
                    vectors.extend_from_slice(r.vector(d));
                }
            }
        }
        Ok((vectors, dim))
    }

    /// The primary keys of the live rows of the table named `table` whose
    /// embedded VECTOR field `field` has no vector (or SPARSE field no
    /// terms): rows written while the
    /// field had no model open here (no API key yet, the model not
    /// fetched), and rows whose sources are all empty, which get none.
    /// [`Db::embed_backfill`] fills in the others.
    pub fn missing_vectors(&self, table: &str, field: &str) -> Result<Vec<Value>> {
        let snap = self.snapshot().table(table).ok_or_else(|| Error::Schema(format!("no table {table}")))?;
        let schema = snap.schema();
        let f = schema.field(field).ok_or_else(|| Error::Schema(format!("{table} has no field {field}")))?;
        if f.embed.is_none() || !matches!(f.ty, FieldType::Vector { .. } | FieldType::Sparse) {
            return Err(Error::Schema(format!("{table}.{field} is not an embedded VECTOR or SPARSE field")));
        }
        let Some(pk) = schema.primary_key else {
            return Err(Error::Schema(format!("{table} has no primary key: rows without {field}'s vector can only be written again")));
        };
        // A SPARSE field's terms are a stored value: the rows whose value is
        // null and that have text to weigh.
        if f.ty == FieldType::Sparse {
            let i = schema.field_index(field).expect("the field");
            let mut keys = Vec::new();
            for d in self.documents(&snap) {
                let d = d?;
                if matches!(d[i], Value::Null) && !schema.embed_text(i, &d).is_empty() {
                    keys.push(d[pk].clone());
                }
            }
            return Ok(keys);
        }
        let key = f.key();
        let mut keys = Vec::new();
        for (i, v) in snap.segments().iter().enumerate() {
            // A segment written before the field existed has no vectors of it.
            let r = match v.file.has(&crate::vector::section(&key, "meta")) {
                true => Some(VectorReader::open(&v.file, &key)?),
                false => None,
            };
            for d in 0..v.file.n_docs() as DocId {
                if v.live.as_ref().is_none_or(|l| l.contains(d)) && !r.as_ref().is_some_and(|r| r.has_vector(d)) {
                    keys.extend(snap.key(i, d));
                }
            }
        }
        Ok(keys)
    }

    /// A write giving the rows of the table named `table` with primary
    /// keys `keys` the vectors of embedded field `field` they lack, from
    /// its model ([`Db::set_embedder_in`]). It is an UPDATE of those rows
    /// which, as it runs under the writer lock, passes over the rows that
    /// have a vector by then and those whose sources are all empty; the
    /// vectors are computed here, before the writer lock, as any write's
    /// are. Run it with [`Db::write_prepared`]; it writes each row again,
    /// its other fields as they are.
    pub fn prepare_backfill(&self, table: &str, field: &str, keys: Vec<Value>) -> Result<PreparedWrite> {
        let schema = self.table(table)?;
        let f = schema.field_index(field).ok_or_else(|| Error::Schema(format!("{table} has no field {field}")))?;
        if self.models_of(&schema)[f].is_none() {
            return Err(Error::Schema(format!("{table}.{field} has no model open here to fill in its vectors")));
        }
        let write = Write::Update { select: qsql::keyed_rows(&schema, keys)?, sets: vec![(f, Assign::Value(Value::Null))] };
        self.prepare_as(schema, write, Some(f))
    }

    /// Fills in the vectors of embedded field `field` of the table named
    /// `table` that its rows lack ([`Db::missing_vectors`]) with the
    /// field's model, `batch` rows a write ([`Db::prepare_backfill`]);
    /// returns the rows written. Rows whose sources are all empty stay
    /// without one. A server does this itself when a field's model opens.
    pub fn embed_backfill(&self, table: &str, field: &str, batch: usize) -> Result<u64> {
        let keys = self.missing_vectors(table, field)?;
        let mut n = 0;
        for chunk in keys.chunks(batch.max(1)) {
            n += self.write_prepared(&self.prepare_backfill(table, field, chunk.to_vec())?)?;
        }
        Ok(n)
    }

    /// Computes the embedded vectors that documents of the first table
    /// leave out (null) from their sources, one batch per field split
    /// across the cores. A document whose sources are all empty gets no
    /// vector. Generated fields must be filled in first.
    pub fn embed_missing(&self, docs: &mut [Document]) -> Result<()> {
        let first = self.schema().table.clone();
        self.embed_missing_in(&first, docs)
    }

    /// [`Db::embed_missing`] for documents of the table named `table`.
    /// The images of a MEDIA field are read from where their values say
    /// ([`crate::media::Resolver::bytes_of`]).
    pub fn embed_missing_in(&self, table: &str, docs: &mut [Document]) -> Result<()> {
        let schema = self.table(table)?;
        embed_with(&self.models_of(&schema), &schema, docs, &Files { resolver: &self.media_resolver(), held: HashMap::new() })
    }

    /// Does for documents of the table named `table` the work a write does
    /// before the writer, so that the rows it logs hold every value: their
    /// MEDIA values are resolved ([`Db::resolve_media_in`]), the derived
    /// text they leave out made (and the generated fields that read it
    /// filled in again), then the embedded vectors they leave out computed
    /// ([`Db::embed_missing_in`]). Generated fields must be filled in
    /// first. [`Db::load`] and SQL writes do this themselves. As a write,
    /// a row that leaves out the vector of a field whose model is not open
    /// is refused, unless [`Db::set_embed_later`] lets it go without.
    pub fn fill_in(&self, table: &str, docs: &mut [Document]) -> Result<()> {
        let schema = self.table(table)?;
        let media = self.media_resolver();
        let files = Files { resolver: &media, held: resolve_media(&media, &schema, docs)? };
        derive_known(&self.derivers_of(&schema), &schema, docs, &Derived::new(), &files)?;
        embed_with(&self.write_models_of(&schema), &schema, docs, &files)
    }

    /// Sets how writes through this `Db` resolve the MEDIA values they give
    /// ([`crate::media::Resolver`]: the sources it may fetch, its size cap
    /// and its media store). The default fetches nothing and keeps
    /// nothing: it takes complete values, and bytes given with a URI.
    pub fn set_media_resolver(&self, r: Resolver) {
        *self.media.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(r);
    }

    /// How writes resolve MEDIA values ([`Db::set_media_resolver`]).
    pub fn media_resolver(&self) -> Arc<Resolver> {
        self.media.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Turns the MEDIA values of documents of the table named `table`, as
    /// writes give them ([`crate::media::input_text`]), into the values
    /// they store ([`crate::media::Resolver::resolve`]): files fetched,
    /// hashed and described here rather than in the writer, so that the
    /// rows a primary logs hold them and replicas never fetch. The first
    /// value that fails fails them all.
    pub fn resolve_media_in(&self, table: &str, docs: &mut [Document]) -> Result<()> {
        let schema = self.table(table)?;
        resolve_media(&self.media_resolver(), &schema, docs).map(drop)
    }

    pub fn query(&self, sql: &str, params: &Params) -> Result<ResultSet> {
        self.query_with(sql, params, &self.exec)
    }

    pub fn query_with(&self, sql: &str, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
        self.query_at(&self.snapshot(), sql, params, opts)
    }

    /// Runs a query against a given snapshot (several queries that must
    /// see the same version share one), on the table it names (`FROM`).
    ///
    /// A compound query (`WITH`, a subquery, a join, `SELECT DISTINCT`) runs
    /// as a plan of operators over single-table queries
    /// ([`crate::query::compound`]), all on the same snapshot; its parts
    /// may read different tables (a join of two tables, a subquery over
    /// another). Its parts may hold up to
    /// [`crate::query::compound::MAX_ROWS`] rows each; its answer is held
    /// to [`ExecOptions::max_rows`].
    pub fn query_at(&self, snap: &Snapshot, sql: &str, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
        // A statement that nests deep runs on a thread with the stack for
        // it, and its planning stops at its deadline as its run does.
        crate::query::parse::deep_stack(crate::query::parse::nests_deep(sql), || crate::query::compound::planning_under(opts, || self.query_at_here(snap, sql, params, opts)))
    }

    /// [`Db::query_at`] on this thread.
    fn query_at_here(&self, snap: &Snapshot, sql: &str, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
        if let Some(call) = crate::query::paths::Call::of_sql(sql)? {
            return self.json_paths_at(snap, &call, params, opts).map(|p| p.to_result());
        }
        // The chunks of a table's text (`FROM chunks_of(table, field)`):
        // the rows it reads, split.
        if let Some(c) = crate::query::chunks::of_sql(sql, params)? {
            let schema = snap.schemas().find(|s| s.table.eq_ignore_ascii_case(&c.table)).ok_or_else(|| Error::Query(format!("no table {}", c.table)))?.clone();
            // Its own read: not held to the answer's `max_rows`, but to a
            // budget of its own, and of as many rows as the LIMIT needs.
            let own = ExecOptions { max_rows: None, memory: crate::query::memory::own_read_budget(opts.memory.as_ref()), ..opts.clone() };
            return c.run(&schema, |source| self.query_at(snap, source, params, &own));
        }
        if crate::query::pack::may_pack(sql) {
            if let Some(p) = self.pack_at(snap, sql, params, opts)? {
                return Ok(p.rs);
            }
        }
        let q = qsql::parse_query_cached(sql)?;
        self.query_parsed_at(snap, sql, q, params, opts)
    }

    /// [`Db::query_at`] of a query parsed (`sql` its text, which is only
    /// looked at for the features it may call), as a step of another reads
    /// it ([`crate::query::pack`]): without printing it and parsing it again.
    fn query_parsed_at(&self, snap: &Snapshot, sql: &str, q: Arc<qsql::Parsed>, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
        // `recommend(...)`: the items a recommender ranks for a user, as
        // `ranked(...)` of their keys, or `FROM recommend(...)`'s rows.
        if crate::query::recommend::may_recommend(sql) {
            if let Some(rs) = self.recommend_query(snap, &q.query, params, opts)? {
                return Ok(rs);
            }
        }
        // Understood words (`bm25(.., understand => 'boost')`): the
        // dictionaries of the table's keyword fields, then the query with
        // the conditions the words name.
        if crate::query::understand::may_understand(sql) {
            if let Some(u) = crate::query::understand::plan(&q.query, params)? {
                let catalog: Vec<Arc<Schema>> = snap.schemas().map(|s| Arc::new(s.clone())).collect();
                let gaz = self.gazetteers_at(snap, &u.needs(&catalog)?, opts, false)?;
                let (rewritten, made) = u.finish(&catalog, &gaz, params, |c| Ok(crate::query::understand::counts_of(&self.query_at(snap, c, &Params::new(), opts)?)))?;
                let mut rs = self.query_at(snap, &rewritten.to_string(), params, opts)?;
                rs.stats.plan.splice(0..0, made.iter().map(|m| m.line()));
                return Ok(rs);
            }
        }
        // Expanded ranking (`bm25(.., expand => n)`): its rows first, then
        // the query with the terms they hold.
        if crate::query::prf::may_expand(sql) {
            if let Some(x) = crate::query::prf::plan(&q.query, params)? {
                let mut rows = Vec::new();
                let schemas: Vec<&Schema> = snap.schemas().collect();
                for f in x.feedback(&schemas)? {
                    rows.push(self.query_at(snap, &f.to_string(), params, opts)?);
                }
                let analyzers = x.fields().iter().map(|(t, f)| {
                    let schema = snap.schemas().find(|s| s.table.eq_ignore_ascii_case(t)).ok_or_else(|| Error::Query(format!("no table {t}")))?;
                    Ok(schema.field(f).ok_or_else(|| Error::Query(format!("{t} has no field {f}")))?.text_analyzer())
                }).collect::<Result<Vec<_>>>()?;
                let (rewritten, made) = x.finish(&rows, &analyzers, params)?;
                let mut rs = self.query_at(snap, &rewritten.to_string(), params, opts)?;
                let lines: Vec<String> = made.iter().map(|m| m.line()).collect();
                rs.stats.plan.splice(0..0, lines);
                return Ok(rs);
            }
        }
        let (q, embedded) = self.embed_texts(snap, sql, q, params)?;
        let params = embedded.as_ref().unwrap_or(params);
        let functions = self.functions_at(snap)?;
        // Calls of a language model: on the rows the query finds.
        if crate::query::llm::may_call(sql, &functions) {
            if let Some(mut split) = crate::query::llm::split(&q.query, &functions, params)? {
                if let Some(m) = opts.max_rows {
                    split.cap_rows(m);
                }
                let plan = plan_of(snap, &split.first, params, &functions)?;
                let rs = self.run_on(snap, &plan, opts)?;
                return split.finish(rs, params, &self.models_dir());
            }
        }
        let plan = plan_of_parsed(snap, &q, params, &functions)?;
        self.run_on(snap, &plan, opts)
    }

    /// The gazetteers `needs` names ([`crate::query::understand`]), read
    /// once per version of their table, each by one caller at a time: the
    /// others wait for it, or, unless `exact`, use the one read before,
    /// which they also use for [`GAZETTEER_FRESH`] after it was read
    /// though the table took writes since (stale while it is read again).
    /// `exact` (a derived field's dictionary, in a write) takes the one of
    /// the table as it is.
    fn gazetteers_at(&self, snap: &Snapshot, needs: &[crate::query::understand::Need], opts: &ExecOptions, exact: bool) -> Result<crate::query::understand::Gazetteers> {
        let mut out = crate::query::understand::Gazetteers::new();
        for need in needs {
            let key = &need.key;
            let stamp = snap.table(&key.0).map_or(0, |t| t.stamp());
            // The one read before, when it holds enough values: whether it
            // is of this version, and it.
            let cached = || {
                let all = self.gazetteers.lock().unwrap_or_else(|e| e.into_inner());
                all.get(key).filter(|c| need.met_by(&c.g)).map(|c| (c.stamp == stamp || (!exact && c.at.elapsed() < GAZETTEER_FRESH), c.g.clone()))
            };
            let before = cached();
            if let Some((true, g)) = &before {
                out.insert(key.clone(), g.clone());
                continue;
            }
            let lock = self.gazetteer_reads.lock().unwrap_or_else(|e| e.into_inner()).entry(key.clone()).or_default().clone();
            let _reading = match lock.try_lock() {
                Ok(g) => g,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    // Another caller reads it: the one before, or wait.
                    if let (Some((_, g)), false) = (&before, exact) {
                        out.insert(key.clone(), g.clone());
                        continue;
                    }
                    lock.lock().unwrap_or_else(|e| e.into_inner())
                }
            };
            // Read by the caller this one waited for.
            if let Some((true, g)) = cached() {
                out.insert(key.clone(), g);
                continue;
            }
            let rs = self.query_at(snap, &need.sql(), &Params::new(), &ExecOptions { max_rows: None, memory: crate::query::memory::own_read_budget(opts.memory.as_ref()), ..opts.clone() })?;
            let g = Arc::new(need.build(&rs));
            self.gazetteers.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), ReadGazetteer { stamp, at: std::time::Instant::now(), g: g.clone() });
            out.insert(key.clone(), g);
        }
        Ok(out)
    }

    /// The gazetteers `needs` names, as of now: what a server that runs
    /// plans rather than SQL passes [`crate::query::understand::Understanding::finish`].
    pub fn gazetteers(&self, needs: &[crate::query::understand::Need]) -> Result<crate::query::understand::Gazetteers> {
        self.gazetteers_at(&self.snapshot(), needs, &self.exec, false)
    }

    /// What the words of `text` say for `field` (a TEXT field) of `table`:
    /// the values of its keyword fields they name and the ranges of numbers
    /// ([`crate::query::understand`]), as `bm25(field, text, understand =>
    /// ..)` would take them. For previews and for applications that show
    /// the filters a search took.
    pub fn understand(&self, table: &str, field: &str, text: &str, opts: &crate::query::understand::Options) -> Result<crate::query::understand::Understood> {
        let snap = self.snapshot();
        let schema = snap.schemas().find(|s| s.table.eq_ignore_ascii_case(table)).cloned().ok_or_else(|| Error::Query(format!("no table {table}")))?;
        let needs = crate::query::understand::needs_of(&schema, field, opts)?;
        let gaz = self.gazetteers_at(&snap, &needs, &self.exec, false)?;
        let mut u = crate::query::understand::understand_in(&schema, field, text, opts, &gaz)?;
        let counts = u.checks().iter().map(|c| Ok(crate::query::understand::counts_of(&self.query_at(&snap, c, &Params::new(), &self.exec)?))).collect::<Result<Vec<_>>>()?;
        u.confirm(&counts);
        Ok(u)
    }

    /// Runs a query that ends in `PACK n TOKENS` ([`crate::query::pack`]):
    /// its rows taken to fill the budget, and the receipt of how they were
    /// chosen. A query without the clause is an error.
    pub fn pack(&self, sql: &str, params: &Params) -> Result<crate::query::pack::Packed> {
        self.pack_at(&self.snapshot(), sql, params, &self.exec)?.ok_or_else(|| Error::Query("the query has no PACK n TOKENS clause".into()))
    }

    /// [`Db::pack`] against a given snapshot, `None` for a query without
    /// the clause. Every slice of candidates, and every row followed, is
    /// read from `snap`.
    pub fn pack_at(&self, snap: &Snapshot, sql: &str, params: &Params, opts: &ExecOptions) -> Result<Option<crate::query::pack::Packed>> {
        let table_of = |t: &str| snap.schemas().find(|s| s.table.eq_ignore_ascii_case(t)).map(|s| Arc::new(s.clone()));
        let Some(mut p) = crate::query::pack::Packer::of_sql(sql, params, &table_of)? else { return Ok(None) };
        while let Some(next) = p.next_query()? {
            // Each read as the query it is, not as its text printed and
            // parsed again; only a table function the text names
            // (`chunks_of`, `json_paths`) is read from its text.
            let text = next.query.to_string();
            let rs = if crate::query::chunks::may_call(&text) || text.to_ascii_lowercase().contains("json_paths") {
                self.query_at(snap, &next.sql()?, &next.params, opts)?
            } else {
                let profiles = text.to_ascii_lowercase().contains("rank_profile");
                self.query_parsed_at(snap, &text, Arc::new(qsql::Parsed { query: next.query, profiles }), &next.params, opts)?
            };
            p.feed(rs)?;
        }
        p.finish(Some(snap.txn()), snap.seq_no()).map(Some)
    }

    /// Where this database's models are: the directory
    /// [`Db::open_embedders`] was given, else [`crate::embed::models_dir`].
    pub fn models_dir(&self) -> std::path::PathBuf {
        self.models.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(crate::embed::models_dir)
    }

    /// Query `q` (of text `sql`) with the texts it gives where vectors go
    /// embedded, and the parameters that hold their vectors
    /// ([`crate::query::embed_sql`]); `q` as it is when it has none.
    fn embed_texts(&self, snap: &Snapshot, sql: &str, q: Arc<qsql::Parsed>, params: &Params) -> Result<(Arc<qsql::Parsed>, Option<Params>)> {
        if !crate::query::embed_sql::may_embed_with(sql, params) {
            return Ok((q, None));
        }
        let catalog: Vec<&Schema> = snap.schemas().collect();
        Ok(match crate::query::embed_sql::rewrite(&q.query, &catalog, params, self)? {
            Some((query, p)) => (Arc::new(qsql::Parsed { query, profiles: q.profiles }), Some(p)),
            None => (q, None),
        })
    }

    /// The model a query names in `embed('model', ..)`: one an embedded
    /// field has open, else one opened from [`Db::models_dir`] and kept,
    /// named as [`crate::query::embed_sql::refuse_model`] allows.
    pub fn named_model(&self, name: &str) -> Result<Arc<dyn Embedder>> {
        if let Some(m) = self.embedders.read().unwrap_or_else(|e| e.into_inner()).iter().find(|m| m.embed.model.eq_ignore_ascii_case(name)) {
            return Ok(m.model.clone());
        }
        if let Some(m) = self.named_models.read().unwrap_or_else(|e| e.into_inner()).get(name) {
            return Ok(m.clone());
        }
        let dir = self.models_dir();
        if let Some(why) = crate::query::embed_sql::refuse_model(name, &dir) {
            return Err(Error::Query(format!("embed('{name}', ..): {why}")));
        }
        let m = crate::embed::open(name, &dir).map_err(|e| match crate::embed::KNOWN_MODELS.iter().any(|k| k.0.eq_ignore_ascii_case(name)) {
            true => Error::Schema(format!("{} (chloedb-server fetch-model {name} downloads it into {})", match e { Error::Schema(m) => m, e => e.to_string() }, dir.display())),
            false => e,
        })?;
        if !m.single() {
            return Err(Error::Schema(format!("model {name} embeds a text into a vector a token, not one vector: embed('{name}', ..) needs a model of one vector a text")));
        }
        let mut kept = self.named_models.write().unwrap_or_else(|e| e.into_inner());
        // A few, as the server keeps: each is a model's tokenizer and table.
        if kept.len() >= 16 {
            kept.clear();
        }
        kept.insert(name.to_string(), m.clone());
        Ok(m)
    }

    /// Runs a query ([`Db::query`]) and logs what it served to a log table
    /// ([`crate::rank_log`]), made by the first query logged to it: one row
    /// per row answered, or per candidate ([`Log::candidates`]), with the
    /// values its ranking read. The answer has the columns the query asked
    /// for. The query reads one snapshot and the log is written after it in
    /// one transaction, as [`Db::sql`] writes. With [`Log::interleave`],
    /// both rankings read the snapshot and the rows shown are their
    /// [`rank_log::interleave`].
    pub fn query_logged(&self, sql: &str, params: &Params, log: &Log) -> Result<Logged> {
        if self.storage.table(&log.table).is_none() {
            self.sql(&rank_log::create_sql(&log.table)?, &Params::new())?;
        }
        let schema = self.table(&log.table)?;
        rank_log::check(&schema)?;
        let p = rank_log::prepare(sql, params, log)?;
        let mut served = match &log.interleave {
            None => rank_log::serve(self.query(&p.sql, params)?, &p, log)?,
            Some(il) => {
                let pb = rank_log::prepare(&il.sql, params, log)?;
                let snap = self.snapshot();
                let a = rank_log::serve(self.query_at(&snap, &p.sql, params, &self.exec)?, &p, log)?;
                let b = rank_log::serve(self.query_at(&snap, &pb.sql, params, &self.exec)?, &pb, log)?;
                rank_log::interleave(a, b, &p, il)?
            }
        };
        rank_log::keep_query(&mut served, &p, &schema);
        let now = crate::timestamp::now();
        let mut t = self.storage.begin()?;
        {
            let mut tt = t.table(&schema.table)?;
            for e in &served.entries {
                tt.upsert(crate::ingest::document_at(&schema, &e.to_json(), now)?)?;
            }
        }
        let seq_no = t.commit_seq()?;
        Ok(Logged { rs: served.rs, request: served.request, seq_no, teams: served.teams })
    }

    /// Runs the relevance autopilot ([`crate::autopilot`]) on one snapshot:
    /// the logged queries that rank by a rank profile replayed with the
    /// profile and with the best numbers its search finds, and the measured
    /// before and after. Reads only; [`Db::keep_proposal`] keeps the report.
    pub fn autopilot(&self, o: &crate::autopilot::Options) -> Result<crate::autopilot::Report> {
        let snap = self.snapshot();
        crate::autopilot::run(self, &snap, &self.exec, o)
    }

    /// Keeps a report of [`Db::autopilot`] in the proposals table
    /// ([`crate::autopilot::PROPOSALS`], made by the first), status
    /// `open`, and returns its key.
    pub fn keep_proposal(&self, r: &crate::autopilot::Report) -> Result<String> {
        use crate::autopilot as ap;
        self.sql(&ap::proposals_sql(ap::PROPOSALS)?, &Params::new())?;
        let now = crate::timestamp::now();
        let id = ap::proposal_id(r, now);
        self.upsert_json(ap::PROPOSALS, &[r.proposal_doc(&id, now)])?;
        Ok(id)
    }

    /// A kept proposal's row: its columns but the proof, or with it.
    pub fn proposal(&self, id: &str, proof: bool) -> Result<serde_json::Value> {
        use crate::autopilot as ap;
        let cols = if proof { "id, target, profile, ts, status, verdict, gain, before, after, baseline_hash, statement, proof" } else { "id, target, profile, ts, status, verdict, gain, before, after, baseline_hash, statement" };
        if self.storage.table(ap::PROPOSALS).is_none() {
            return Err(Error::Query(format!("no proposal {id}: none was kept")));
        }
        let rs = self.query(&format!("SELECT {cols} FROM {} WHERE id = :id", ap::PROPOSALS), &Params::new().set("id", Value::Str(id.into())))?;
        let row = rs.rows.first().ok_or_else(|| Error::Query(format!("no proposal {id}")))?;
        Ok(serde_json::Value::Object(rs.columns.iter().zip(row).map(|(c, v)| (c.clone(), crate::autopilot::cell_json(v))).collect()))
    }

    /// Applies kept proposal `id` ([`crate::autopilot::check_apply`]): its
    /// `CREATE OR REPLACE RANK PROFILE`, while the profile is the one it was
    /// measured against, and marks it `applied`.
    pub fn apply_proposal(&self, id: &str, force: bool) -> Result<()> {
        let row = self.proposal(id, true)?;
        let statement = self.proposal_statement(&row, force)?;
        self.sql(&statement, &Params::new())?;
        self.set_proposal_status(id, "applied")
    }

    /// The statement that applies kept proposal row `row` (with its
    /// proof), once [`crate::autopilot::check_apply`] took it and its proof,
    /// made again on one snapshot now ([`crate::autopilot::reprove`]), still
    /// proposes it ([`crate::autopilot::check_reproof`]; `force` applies it
    /// whatever the verdict, never a ranking but the profile's own).
    pub fn proposal_statement(&self, row: &serde_json::Value, force: bool) -> Result<String> {
        use crate::autopilot as ap;
        let s = |k: &str| row.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let snap = self.snapshot();
        let now = snap.table(&s("target")).and_then(|t| t.rank_profile(&s("profile")).cloned());
        let (candidate, statement) = ap::check_apply(row, now.as_ref(), force)?;
        let again = ap::reprove(self, &snap, &self.exec, &ap::Options::of_proposal(row)?, &candidate)?;
        ap::check_reproof(row, &again, force)?;
        Ok(statement)
    }

    /// Marks kept proposal `id` `dismissed` (or another status).
    pub fn set_proposal_status(&self, id: &str, status: &str) -> Result<()> {
        self.proposal(id, false)?;
        self.sql(&format!("UPDATE {} SET status = :s WHERE id = :id", crate::autopilot::PROPOSALS), &Params::new().set("s", Value::Str(status.into())).set("id", Value::Str(id.into())))?;
        Ok(())
    }

    /// Writes judgments ([`crate::autopilot::judgment_doc`]) to judgments
    /// table `table`, made by the first write.
    pub fn add_judgments(&self, table: &str, docs: &[serde_json::Value]) -> Result<()> {
        self.sql(&crate::autopilot::judgments_sql(table)?, &Params::new())?;
        self.upsert_json(table, docs)
    }

    /// Upserts JSON documents to `table` in one transaction.
    fn upsert_json(&self, table: &str, docs: &[serde_json::Value]) -> Result<()> {
        let schema = self.table(table)?;
        let now = crate::timestamp::now();
        let mut t = self.storage.begin()?;
        {
            let mut tt = t.table(&schema.table)?;
            for d in docs {
                tt.upsert(crate::ingest::document_at(&schema, d, now)?)?;
            }
        }
        t.commit_seq()?;
        Ok(())
    }

    /// The credit of each pair of interleaved rankings in a log table
    /// ([`rank_log::Credits`]), over its export `x` ([`Db::export_rank_log`]),
    /// whose events and window label the rows acted on.
    pub fn interleaving_credit(&self, x: &Export) -> Result<Vec<rank_log::Credit>> {
        let mut c = rank_log::Credits::default();
        self.export_rank_log(x, |r| {
            c.add(&r);
            Ok(())
        })?;
        Ok(c.finish())
    }

    /// Exports a log table ([`rank_log::export`]) from one snapshot: one
    /// record per logged row, with its features as served and, from an
    /// events table, its history and label as of its time. Returns the
    /// records given to `out`.
    pub fn export_rank_log(&self, x: &Export, mut out: impl FnMut(serde_json::Value) -> Result<()>) -> Result<u64> {
        let snap = self.snapshot();
        let tables: Vec<&Schema> = snap.schemas().collect();
        let opts = ExecOptions { max_rows: None, ..self.exec.clone() };
        rank_log::export(&mut |sql, params| self.query_at(&snap, sql, params, &opts), &tables, x, &mut out)
    }

    /// Runs a read plan on `snap`'s version: a leaf on the executor, any
    /// other with operators ([`crate::query::ops`]), each of whose leaves
    /// reads its own table and may hold up to
    /// [`crate::query::compound::MAX_ROWS`] rows.
    fn run_on(&self, snap: &Snapshot, plan: &Plan, opts: &ExecOptions) -> Result<ResultSet> {
        match self.run_leaf_on(snap, plan, opts, None)? {
            Leaf::Rows(rs) => Ok(rs),
            #[cfg(feature = "datafusion")]
            Leaf::Arrow(_) => unreachable!("rows asked for"),
        }
    }

    /// [`Db::query`], its answer as Arrow columns where the executor makes
    /// them (feature `datafusion`): a query of one table, that neither
    /// groups nor counts, has its rows' columns read from its segments into
    /// Arrow arrays ([`Leaf::Arrow`], [`Db::execute_leaf`]) without being
    /// made rows first. Any other answer is its rows ([`Leaf::Rows`]).
    #[cfg(feature = "datafusion")]
    pub fn query_leaf(&self, sql: &str, params: &Params) -> Result<Leaf> {
        self.query_leaf_with(sql, params, &self.exec)
    }

    /// [`Db::query_leaf`] with `opts`, as [`Db::query_with`].
    #[cfg(feature = "datafusion")]
    pub fn query_leaf_with(&self, sql: &str, params: &Params, opts: &ExecOptions) -> Result<Leaf> {
        let snap = self.snapshot();
        if let Some(call) = crate::query::paths::Call::of_sql(sql)? {
            return self.json_paths_at(&snap, &call, params, opts).map(|p| Leaf::Rows(p.to_result()));
        }
        // A statement that is run as steps (PACK, chunks_of, understood or
        // expanded words, llm()): its rows, as Db::query answers them.
        let functions = self.functions_at(&snap)?;
        let stepped = (crate::query::pack::may_pack(sql) && crate::query::pack::split(sql)?.is_some())
            || crate::query::chunks::may_call(sql)
            || crate::query::understand::may_understand(sql)
            || crate::query::prf::may_expand(sql)
            || crate::query::llm::may_call(sql, &functions);
        if stepped {
            return self.query_at(&snap, sql, params, opts).map(Leaf::Rows);
        }
        let q = qsql::parse_query_cached(sql)?;
        let (q, embedded) = self.embed_texts(&snap, sql, q, params)?;
        let params = embedded.as_ref().unwrap_or(params);
        let plan = plan_of_parsed(&snap, &q, params, &functions)?;
        self.run_leaf_on(&snap, &plan, opts, Some(0))
    }

    /// [`Db::run_on`], a plan of one table's rows as Arrow columns when
    /// there are more than `arrow` of them.
    fn run_leaf_on(&self, snap: &Snapshot, plan: &Plan, opts: &ExecOptions, arrow: Option<usize>) -> Result<Leaf> {
        profile::check_plan(plan, &|t, name| snap.table(t).and_then(|v| v.rank_profile(name).map(|p| p.hash())))?;
        let opened;
        let plan = match plan.root.has_models() {
            true => {
                let mut p = plan.clone();
                self.open_ranking_models(&mut p, false)?;
                opened = p;
                &opened
            }
            false => plan,
        };
        let view_of = |table: &str| snap.table(table).ok_or_else(|| Error::Query(format!("no such table: {table}")));
        if !plan.root.uses_operators() {
            let view = view_of(plan.table())?;
            let select = plan.to_select(view.schema())?;
            return match arrow {
                Some(over) => self.execute_leaf(&view, &select, opts, over),
                None => self.execute(&view, &select, opts).map(Leaf::Rows),
            };
        }
        let leaves = ExecOptions { max_rows: None, ..opts.clone() };
        let catalog: Vec<&Schema> = snap.schemas().collect();
        let mut runner = |leaf: &Plan, over: usize| {
            let view = view_of(leaf.table())?;
            self.execute_leaf(&view, &leaf.to_select(view.schema())?, &leaves, over)
        };
        let rs = ops::run_on_engine(plan, &catalog, &mut runner, opts)?;
        if let Some(max) = opts.max_rows.filter(|&m| rs.rows.len() > m) {
            return Err(Error::Query(format!("the query matches more than {max} rows, the most a query returns; add a LIMIT")));
        }
        Ok(Leaf::Rows(rs))
    }

    /// Runs a plan ([`crate::query::ir`]) on the table it names: a read
    /// returns its rows, a write the number of rows it wrote, in one
    /// transaction. SQL runs the same way: [`Db::query`] and [`Db::sql`]
    /// bind a statement into a plan, and the executor answers its Search.
    /// A plan made elsewhere is checked against the table's schema as the
    /// binder checks SQL.
    pub fn run(&self, plan: &Plan) -> Result<SqlResult> {
        self.run_with(plan, &self.exec)
    }

    /// [`Db::run`], a plan that reads run with `opts` (its deadline, cancel
    /// flag and memory budget); a write runs as [`Db::run`] runs it.
    pub fn run_with(&self, plan: &Plan, opts: &ExecOptions) -> Result<SqlResult> {
        match plan.is_write() {
            true => self.write_queued(writes::Job::Sql(self.prepare_plan(plan)?)).map(|(n, _)| SqlResult::Written(n)),
            false => self.run_at(&self.snapshot(), plan, opts).map(SqlResult::Rows),
        }
    }

    /// [`Db::run`] of a plan that reads, against a given snapshot.
    pub fn run_at(&self, snap: &Snapshot, plan: &Plan, opts: &ExecOptions) -> Result<ResultSet> {
        self.run_on(snap, plan, opts)
    }

    /// [`Db::prepare_write`] of a plan that writes.
    pub fn prepare_plan(&self, plan: &Plan) -> Result<PreparedWrite> {
        if !plan.is_write() {
            return Err(Error::Query("the plan reads; run it with Db::run".into()));
        }
        let schema = self.table(plan.table())?;
        let write = plan.to_write(&schema)?;
        self.prepare(schema, write)
    }

    /// Runs any statement, on any table: a SELECT returns its rows; an
    /// INSERT, UPDATE or DELETE the number of documents it wrote, in one
    /// transaction; `CREATE TABLE` ([`Db::create_table`]) and `DROP TABLE`
    /// ([`Db::drop_tables`]) 0.
    ///
    /// UPDATE sets numeric fields in place ([`crate::storage::WriteTxn::set`])
    /// and rewrites the whole document for any other field, which needs every
    /// text field stored or generated. The rows to change are read after the
    /// transaction takes the writer lock, so no other write lands between.
    pub fn sql(&self, sql: &str, params: &Params) -> Result<SqlResult> {
        self.sql_with(sql, params, &self.exec)
    }

    /// [`Db::sql`], a SELECT run with `opts` (its deadline, cancel flag and
    /// memory budget); any other statement runs as [`Db::sql`] runs it.
    pub fn sql_with(&self, sql: &str, params: &Params, opts: &ExecOptions) -> Result<SqlResult> {
        // A statement that nests deep runs on a thread with the stack for
        // it, and its planning stops at its deadline as its run does.
        crate::query::parse::deep_stack(crate::query::parse::nests_deep(sql), || crate::query::compound::planning_under(opts, || self.sql_with_here(sql, params, opts)))
    }

    /// [`Db::sql_with`] on this thread.
    fn sql_with_here(&self, sql: &str, params: &Params, opts: &ExecOptions) -> Result<SqlResult> {
        if let Some(st) = crate::query::classify::parse_statement(sql)? {
            return self.classifier_statement(&st, opts);
        }
        if let Some(st) = crate::query::recommend::parse_statement(sql)? {
            return self.recommender_statement(&st, opts);
        }
        // `CREATE ROLLUP`, `REFRESH ROLLUP`, `DROP ROLLUP`.
        if let Some(st) = crate::query::rollup::parse_statement(sql)? {
            return self.rollup_statement(st, opts);
        }
        // `CREATE SYNONYMS ON t AS '...'`: the table's synonym list, kept
        // as a function.
        if let Some(st) = crate::query::table_synonyms::parse_statement(sql)? {
            self.table(st.table())?;
            return self.sql_with(&st.function_sql()?, &Params::new(), opts);
        }
        // `INSERT INTO t (...) SELECT ... FROM chunks_of(...)`: the chunks,
        // then their INSERTs.
        if let Some(ins) = crate::query::chunks::insert_of_sql(sql, params)? {
            let schema = self.table(&ins.query.table)?;
            let own = ExecOptions { max_rows: None, memory: crate::query::memory::own_read_budget(opts.memory.as_ref()), ..opts.clone() };
            let chunks = ins.query.run(&schema, |source| self.query_with(source, params, &own))?;
            let mut n = 0;
            for (statement, p) in ins.inserts(&chunks) {
                if let SqlResult::Written(w) = self.sql_with(&statement, &p, opts)? {
                    n += w;
                }
            }
            return Ok(SqlResult::Written(n));
        }
        let catalog = self.catalog();
        let tables: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
        match bind_command(sql, &tables, params)? {
            Command::Query => self.query_with(sql, params, opts).map(SqlResult::Rows),
            Command::Explain { json } => {
                let sqlparser::ast::Statement::Explain { statement, .. } = qsql::parse_one(sql)? else { unreachable!("bound as EXPLAIN") };
                let plan = self.explain_statement(&statement, params)?;
                let rows = match json {
                    true => vec![vec![Value::Str(plan.to_json_text()?)]],
                    false => {
                        let mut lines = plan.explain(&tables);
                        // The plan is the query's with the calls written out.
                        if let sqlparser::ast::Statement::Query(q) = &*statement {
                            if let Some((_, used)) = function::expand(q, &*self.functions()?)? {
                                lines.insert(0, format!("calls stored function{} {}", if used.len() == 1 { "" } else { "s" }, used.join(", ")));
                            }
                        }
                        lines.into_iter().map(|l| vec![Value::Str(l)]).collect()
                    }
                };
                let stats = ExecStats::default();
                Ok(SqlResult::Rows(ResultSet { columns: vec!["plan".into()], rows, keys: Vec::new(), scores: Vec::new(), parts: None, stats }))
            }
            Command::CreateTable { schema, if_not_exists } => {
                if !(if_not_exists && self.storage.table(&schema.table).is_some()) {
                    self.create_table(schema)?;
                }
                Ok(SqlResult::Written(0))
            }
            Command::DropTable { tables, opts } => {
                let names: Vec<&str> = tables.iter().map(|t| t.as_str()).collect();
                self.drop_tables(&names, opts)?;
                Ok(SqlResult::Written(0))
            }
            Command::AlterTable { table, alters } => {
                if let Some(t) = table.filter(|_| !alters.is_empty()) {
                    self.alter_table(&t, &alters)?;
                }
                Ok(SqlResult::Written(0))
            }
            Command::CreateRankProfile { table, profile, replace } => {
                self.storage.set_rank_profile(&table, profile, replace)?;
                Ok(SqlResult::Written(0))
            }
            Command::Function(stmt) => {
                self.define_function(&stmt, None)?;
                Ok(SqlResult::Written(0))
            }
            Command::DropRankProfile { table, name, if_exists } => {
                let table = match table {
                    Some(t) => Some(t),
                    None => {
                        let mut with: Vec<String> = Vec::new();
                        for t in &catalog {
                            if self.storage.rank_profiles(&t.table)?.iter().any(|p| p.name.eq_ignore_ascii_case(&name)) {
                                with.push(t.table.clone());
                            }
                        }
                        if with.len() > 1 {
                            return Err(Error::Query(format!("{} have a rank profile named {name}: DROP RANK PROFILE {name} ON table", with.join(" and "))));
                        }
                        with.pop()
                    }
                };
                let dropped = match &table {
                    Some(t) => self.storage.drop_rank_profile(t, &name)?,
                    None => false,
                };
                if !dropped && !if_exists {
                    return Err(Error::Query(format!("no rank profile named {name}{}", table.map_or(String::new(), |t| format!(" on {t}")))));
                }
                Ok(SqlResult::Written(0))
            }
            Command::Write { table, write } => {
                let schema = catalog.iter().find(|t| t.table == table).expect("bound to a table of the catalog");
                let write = Plan::of_write(schema, &write).to_write(schema)?;
                let mut r = self.write_queued(writes::Job::Sql(self.prepare(schema.clone(), write)?));
                // An ALTER TABLE committed between binding the statement and
                // writing it: it is bound again, to the table as it is now.
                for _ in 0..ALTERED_TRIES {
                    if !r.as_ref().is_err_and(|e| altered_while_running(&e.to_string())) {
                        break;
                    }
                    let Some(p) = self.prepare_write(sql, params)? else { break };
                    r = self.write_queued(writes::Job::Sql(p));
                }
                r.map(|(n, _)| SqlResult::Written(n))
            }
        }
    }

    /// The plan of one statement ([`crate::query::ir`]): a SELECT, INSERT,
    /// UPDATE or DELETE, bound as it would run but not run. `EXPLAIN
    /// <statement>` in [`Db::sql`] gives it as text or JSON. A compound
    /// SELECT (`WITH`, a subquery, a join, `SELECT DISTINCT`) plans into
    /// operators over single-table queries ([`crate::query::compound`]).
    pub fn explain(&self, sql: &str, params: &Params) -> Result<Plan> {
        self.explain_statement(&qsql::parse_one(sql)?, params)
    }

    /// `CREATE CLASSIFIER`, which trains on the table's rows and keeps the
    /// classifier as a stored function, answering what the training
    /// measured; or `DROP CLASSIFIER`, that function's drop
    /// ([`crate::query::classify`]).
    fn classifier_statement(&self, st: &crate::query::classify::Statement, opts: &ExecOptions) -> Result<SqlResult> {
        use crate::query::classify::Statement;
        match st {
            Statement::Drop { .. } => self.sql_with(&st.drop_sql().expect("a drop"), &Params::new(), opts),
            Statement::Create(c) => {
                let schema = self.table(&c.table)?;
                c.check(&schema)?;
                if !c.replace && self.functions()?.current(&c.name).is_some() {
                    return Err(Error::Query(format!("function {} exists: CREATE OR REPLACE CLASSIFIER {} makes its next version", c.name, c.name)));
                }
                let rs = self.query_with(&c.training_sql(), &Params::new(), &ExecOptions { max_rows: None, memory: crate::query::memory::own_read_budget(opts.memory.as_ref()), ..opts.clone() })?;
                let trained = c.train(&schema, &rs)?;
                self.sql_with(&trained.function_sql(c), &Params::new(), opts)?;
                Ok(SqlResult::Rows(trained.result_set()))
            }
        }
    }

    /// Synonyms mined from table `req.table` ([`crate::query::mine`]):
    /// proposals for its synonym list, which nothing applies until a
    /// `CREATE SYNONYMS` takes them.
    pub fn mine_synonyms(&self, req: &crate::query::mine::Request) -> Result<crate::query::mine::Mined> {
        use crate::query::mine;
        let t0 = std::time::Instant::now();
        let schema = self.table(&req.table)?;
        let f = schema.field(&req.field).ok_or_else(|| Error::Query(format!("{} has no field {}", req.table, req.field)))?;
        if !matches!(f.ty, FieldType::Text { .. }) {
            return Err(Error::Query(format!("{}.{} is not a TEXT field: synonyms are of a text's words", req.table, req.field)));
        }
        // Within the bounds a mining keeps to, whoever asks.
        let bounded = req.options.bounded();
        let (analyzer, opts) = (f.text_analyzer(), &bounded);
        let (source, rows, words, proposals, header) = match &req.source {
            mine::Source::Vectors { vector } => {
                if !f.stored {
                    return Err(Error::Query(format!("{}.{} is not stored: its words are read from the stored text", req.table, req.field)));
                }
                let e = self.embedder_in(&req.table, vector).ok_or_else(|| Error::Query(format!("{}.{vector} has no model here to embed words with", req.table)))?;
                let sql = format!("SELECT {} FROM {} LIMIT {}", qsql::ident(&f.name), qsql::ident(&schema.table), opts.max_rows);
                let rs = self.query_with(&sql, &Params::new(), &ExecOptions { max_rows: None, memory: crate::query::memory::own_read_budget(self.exec.memory.as_ref()), ..self.exec.clone() })?;
                let (rows, words) = mine::vocabulary(rs.rows.iter().filter_map(|r| r.first().and_then(|v| v.as_str())), analyzer, opts);
                let vectors = words.iter().map(|w| e.embed_query(&w.word)).collect::<Result<Vec<_>>>()?;
                let ps = mine::by_vectors(&words, &vectors, opts);
                let header = format!("mined from the words of {}.{}, embedded by the model of {vector}: cosine {} or more", req.table, req.field, opts.min_cos);
                ("vectors", rows, words.len(), ps, header)
            }
            mine::Source::Log { log } => {
                let sql = format!("SELECT context, ts, text FROM {} LIMIT {}", qsql::ident(log), opts.max_rows);
                let rs = self.query_with(&sql, &Params::new(), &ExecOptions { max_rows: None, memory: crate::query::memory::own_read_budget(self.exec.memory.as_ref()), ..self.exec.clone() })?;
                let searches: Vec<mine::Search> = rs
                    .rows
                    .iter()
                    .filter_map(|r| {
                        let ctx = match &r[0] {
                            Value::Null => return None,
                            Value::Str(s) => s.clone(),
                            v => crate::query::hint::sql_value(v),
                        };
                        Some((ctx, r[1].as_i64()?, r[2].as_str()?.to_string()))
                    })
                    .collect();
                let (words, ps) = mine::by_log(&searches, analyzer, opts);
                let header = format!("mined from the searches of {log}: one-word rewrites within {} seconds, by {} contexts or more", opts.window, opts.min_support);
                ("log", searches.len(), words, ps, header)
            }
        };
        Ok(mine::Mined {
            source: source.into(),
            table: req.table.clone(),
            field: req.field.clone(),
            rows,
            words,
            rules: mine::rules(&header, &proposals),
            proposals,
            took_ms: t0.elapsed().as_secs_f64() * 1e3,
        })
    }

    /// The classes of `text` by the classifier `name` (a stored function
    /// `CREATE CLASSIFIER` made), its `k` best labels: the text embedded by
    /// the model of the field the classifier was trained on.
    pub fn classify(&self, name: &str, text: &str, k: usize) -> Result<crate::query::classify::Classified> {
        let functions = self.functions()?;
        let f = functions.current(name).ok_or_else(|| Error::Query(format!("no classifier named {name}")))?;
        let c = crate::query::classify::of_function(f)?.ok_or_else(|| Error::Query(format!("function {name} is not a classifier CREATE CLASSIFIER made")))?;
        let Value::Vector(x) = self.embed_query_in(&c.table, &c.field, text)? else {
            return Err(Error::Query(format!("{}.{} holds several vectors a row", c.table, c.field)));
        };
        crate::query::classify::Classified::of(name, f.version, &c, &x, k)
    }

    fn explain_statement(&self, stmt: &sqlparser::ast::Statement, params: &Params) -> Result<Plan> {
        if let sqlparser::ast::Statement::Query(q) = stmt {
            // The plan names the files of its ranking models.
            let snap = self.snapshot();
            let functions = self.functions_at(&snap)?;
            // Texts embedded where vectors go, as the query runs them.
            let text = q.to_string();
            let embedded = match crate::query::embed_sql::may_embed_with(&text, params) {
                true => crate::query::embed_sql::rewrite(q, &snap.schemas().collect::<Vec<_>>(), params, self)?,
                false => None,
            };
            let (q, params) = match &embedded {
                Some((q, p)) => (q, p),
                None => (&**q, params),
            };
            // A query that calls a language model: the plan of the rows it
            // asks about.
            let split = match crate::query::llm::may_call(&q.to_string(), &functions) {
                true => crate::query::llm::split(q, &functions, params)?,
                false => None,
            };
            let mut plan = plan_of(&snap, split.as_ref().map_or(q, |s| &s.first), params, &functions)?;
            self.open_ranking_models(&mut plan, false)?;
            return Ok(plan);
        }
        let catalog = self.catalog();
        let tables: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
        match qsql::bind_statement(stmt, &tables, params)? {
            Command::Write { table, write } => {
                let schema = catalog.iter().find(|t| t.table == table).expect("bound to a table of the catalog");
                Ok(Plan::of_write(schema, &write))
            }
            _ => Err(Error::Query("EXPLAIN shows the plan of a SELECT, INSERT, UPDATE or DELETE".into())),
        }
    }

    /// Binds an INSERT, UPDATE or DELETE, and computes the vectors it
    /// writes (of the embedded fields it leaves out, and of those whose
    /// sources an UPDATE changes) before [`Db::write_prepared`] takes the
    /// writer lock: a model is slow, and every other write waits while the
    /// lock is held. `None` for any other statement. [`Db::sql`] does both.
    pub fn prepare_write(&self, sql: &str, params: &Params) -> Result<Option<PreparedWrite>> {
        let catalog = self.catalog();
        let tables: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
        let Command::Write { table, write } = bind_command(sql, &tables, params)? else { return Ok(None) };
        let schema = catalog.iter().find(|t| t.table == table).expect("bound to a table of the catalog");
        let write = Plan::of_write(schema, &write).to_write(schema)?;
        Ok(Some(self.prepare(schema.clone(), write)?))
    }

    fn prepare(&self, schema: Arc<Schema>, write: Write) -> Result<PreparedWrite> {
        self.prepare_as(schema, write, None)
    }

    fn prepare_as(&self, schema: Arc<Schema>, mut write: Write, backfill: Option<usize>) -> Result<PreparedWrite> {
        let media = self.media_resolver();
        let mut held = Held::new();
        match &mut write {
            Write::Insert { docs, conflict } => {
                docs.iter_mut().for_each(|d| schema.fill_generated(d));
                held = resolve_media(&media, &schema, docs)?;
                if let Conflict::Update(sets) = conflict {
                    held.extend(resolve_sets(&media, &schema, sets)?);
                }
            }
            Write::Update { sets, .. } => held = resolve_sets(&media, &schema, sets)?,
            Write::Delete { .. } => {}
        }
        let (known, derived) = self.embed_ahead(&schema, &write, &Files { resolver: &media, held }, backfill)?;
        Ok(PreparedWrite { schema, write, known, derived, mask: None, guard: None, backfill })
    }

    /// The text `write` will derive and the vectors it will compute, from
    /// the latest version. The rows an UPDATE or `ON CONFLICT DO UPDATE`
    /// rewrites are read here when what it sets may need a model
    /// ([`sets_need_models`]), and again under the writer lock, where only
    /// the files and texts that changed in between go to a model; rows
    /// named by key are read by key both times ([`Db::by_key`]). An error
    /// reading the rows (the table dropped, a sum that overflows) is left
    /// for the write to report; an image that cannot be read fails the
    /// statement.
    fn embed_ahead(&self, schema: &Schema, write: &Write, files: &Files, backfill: Option<usize>) -> Result<(Known, Derived)> {
        let models = self.write_models_of(schema);
        let derivers = self.derivers_of(schema);
        if models.iter().all(Option::is_none) && derivers.iter().all(Option::is_none) {
            return Ok((Known::new(), Derived::new()));
        }
        let docs = || -> Result<Vec<Document>> {
            let Some(snap) = self.snapshot().table(&schema.table) else { return Ok(Vec::new()) };
            let pk = || schema.primary_key.ok_or_else(|| Error::Query(format!("{} has no primary key", schema.table)));
            Ok(match write {
                Write::Insert { docs, conflict: Conflict::Fail | Conflict::Replace } => docs.clone(),
                Write::Insert { docs, conflict: Conflict::Skip } => not_live(&snap, pk()?, docs.clone()),
                Write::Insert { docs, conflict: Conflict::Update(sets) } => {
                    let pk = pk()?;
                    let (live, mut new): (Vec<Document>, Vec<Document>) = docs.iter().cloned().partition(|d| snap.find(&d[pk]).is_some());
                    if !in_place(schema, sets) && sets_need_models(schema, &models, &derivers, sets) {
                        new.extend(plan_sets(schema, &models, &derivers, self.conflict_rows(&snap, schema, pk, live)?, sets)?.rewrites);
                    }
                    new
                }
                // Numbers set in place embed nothing, nor do fields no
                // model makes, or reads from.
                Write::Update { sets, .. } if in_place(schema, sets) || !sets_need_models(schema, &models, &derivers, sets) => Vec::new(),
                Write::Update { select, sets } => {
                    let rows = lacking(schema, backfill, self.execute(&snap, select, &self.exec)?.rows);
                    plan_sets(schema, &models, &derivers, rows.into_iter().map(|r| (r, Vec::new())).collect(), sets)?.rewrites
                }
                Write::Delete { .. } => Vec::new(),
            })
        };
        let Ok(mut docs) = docs() else { return Ok((Known::new(), Derived::new())) };
        let derived = derived_of(&derivers, schema, &docs, files)?;
        derive_known(&derivers, schema, &mut docs, &derived, files)?;
        Ok((vectors_of(&models, schema, &docs, files)?, derived))
    }

    /// Runs a write [`Db::prepare_write`] bound, in one transaction, and
    /// returns the number of documents written. The rows an UPDATE or
    /// DELETE changes are read under the writer lock, so no other write
    /// lands between; vectors computed ahead are used where their source
    /// texts did not change. Threads writing at once share commits, as
    /// [`Db::write_ops`] says. It can run again after an error (a busy
    /// writer).
    pub fn write_prepared(&self, p: &PreparedWrite) -> Result<u64> {
        Ok(self.write_queued(writes::Job::Sql(p.clone()))?.0)
    }

    /// [`Db::write_prepared`] in a transaction of its own, not shared with
    /// other threads' writes, also returning the transaction's sequence
    /// number in a database that numbers its writes (none if it wrote
    /// nothing): for a caller that orders and groups its writes itself,
    /// as chloedb-server's writer thread does.
    pub fn write_prepared_seq(&self, p: &PreparedWrite) -> Result<(u64, Option<u64>)> {
        let mut t = self.storage.begin()?;
        let n = self.write_prepared_in(&mut t, p)?;
        Ok((n, t.commit_seq()?))
    }

    /// Runs writes [`Db::prepare_write`] bound in one transaction, one
    /// commit for all, and returns the rows each wrote and the
    /// transaction's sequence number. They must be writes that can share
    /// one ([`Footprints::admit`]: no two touch the same rows), else none
    /// runs. If one fails, none is written, and each can run again alone.
    pub fn write_prepared_group(&self, ps: &[&PreparedWrite]) -> Result<(Vec<u64>, Option<u64>)> {
        self.write_prepared_group_then(ps, |_, _| Ok(()))
    }

    /// [`Db::write_prepared_group`], then `then` in the same transaction,
    /// given the rows each write wrote: what a caller keeps of the writes
    /// (chloedb-server keeps their idempotency keys in [`SOURCES`]), so
    /// that it commits with them or not at all.
    pub fn write_prepared_group_then(&self, ps: &[&PreparedWrite], then: impl FnOnce(&mut WriteTxn<'_>, &[u64]) -> Result<()>) -> Result<(Vec<u64>, Option<u64>)> {
        let mut prints = Footprints::default();
        if !ps.iter().all(|p| prints.admit(p)) {
            return Err(Error::Query("these writes cannot share a transaction: two of them may touch the same rows".into()));
        }
        let mut t = self.storage.begin()?;
        let rows = ps.iter().map(|p| self.write_prepared_in(&mut t, p)).collect::<Result<Vec<u64>>>()?;
        then(&mut t, &rows)?;
        Ok((rows, t.commit_seq()?))
    }

    /// Runs a write in transaction `t`, which holds the writer lock. It
    /// finds its rows in the last committed snapshot, which does not show
    /// `t`'s own changes: writes sharing `t` must not touch the same rows
    /// ([`Footprints`]).
    fn write_prepared_in(&self, t: &mut WriteTxn<'_>, p: &PreparedWrite) -> Result<u64> {
        let schema = &*p.schema;
        let mut tt = t.table(&schema.table)?;
        if tt.schema() != schema {
            return Err(Error::Query(format!("table {} was altered, or dropped and created again, {ALTERED}", schema.table)));
        }
        let models = self.write_models_of(schema);
        let derivers = self.derivers_of(schema);
        // Text and vectors computed ahead cover the rows unless they changed
        // since: files of those are read here.
        let media = self.media_resolver();
        let files = Files { resolver: &media, held: Held::new() };
        // Read under the writer lock: no other write lands between.
        let guarded = match &p.guard {
            Some(g) => (g.0)()?,
            None => None,
        };
        let mut snap = self.snapshot();
        if !matches!(p.write, Write::Insert { .. }) {
            for m in guarded.iter().chain(&p.mask) {
                snap = snap.masked(m)?;
            }
        }
        let snap = snap.table(&schema.table).expect("the writer lock keeps the table");
        let pk = || schema.primary_key.ok_or_else(|| Error::Query(format!("{} has no primary key", schema.table)));
        let mut n = 0u64;
        match p.write.clone() {
            Write::Insert { mut docs, conflict } => {
                match conflict {
                    Conflict::Fail | Conflict::Replace => {
                        derive_known(&derivers, schema, &mut docs, &p.derived, &files)?;
                        embed_known(&models, schema, &mut docs, &p.known, &files)?;
                        for d in docs {
                            if conflict == Conflict::Replace {
                                tt.upsert(d)?;
                            } else {
                                tt.insert(d)?;
                            }
                            n += 1;
                        }
                    }
                    Conflict::Skip => {
                        let mut docs = not_live(&snap, pk()?, docs);
                        derive_known(&derivers, schema, &mut docs, &p.derived, &files)?;
                        embed_known(&models, schema, &mut docs, &p.known, &files)?;
                        for d in docs {
                            tt.insert(d)?;
                            n += 1;
                        }
                    }
                    Conflict::Update(sets) => {
                        let pk = pk()?;
                        let mut seen = HashSet::new();
                        if let Some(d) = docs.iter().find(|d| !seen.insert(Part::of(&d[pk]))) {
                            return Err(Error::Query(format!("ON CONFLICT DO UPDATE: key {} comes twice; a statement updates a row once", crate::query::hint::sql_value(&d[pk]))));
                        }
                        let (live, mut new): (Vec<Document>, Vec<Document>) = docs.into_iter().partition(|d| snap.find(&d[pk]).is_some());
                        derive_known(&derivers, schema, &mut new, &p.derived, &files)?;
                        embed_known(&models, schema, &mut new, &p.known, &files)?;
                        for d in new {
                            tt.insert(d)?;
                            n += 1;
                        }
                        let pairs = self.conflict_rows(&snap, schema, pk, live)?;
                        n += set_rows(&mut tt, schema, (&models, &derivers), pairs, &sets, (&p.known, &p.derived), &files)?;
                    }
                }
            }
            Write::Delete { select } => {
                let pk = pk()?;
                for row in self.execute(&snap, &select, &self.exec)?.rows {
                    n += tt.delete(&row[pk])? as u64;
                }
            }
            Write::Update { select, sets } => {
                let rows = lacking(schema, p.backfill, self.execute(&snap, &select, &self.exec)?.rows);
                n += set_rows(&mut tt, schema, (&models, &derivers), rows.into_iter().map(|r| (r, Vec::new())).collect(), &sets, (&p.known, &p.derived), &files)?;
            }
        }
        Ok(n)
    }

    /// The live rows of the documents an INSERT proposed (`live`), every
    /// field, each paired with its document, for `ON CONFLICT DO UPDATE`.
    fn conflict_rows(&self, snap: &Snapshot, schema: &Schema, pk: usize, live: Vec<Document>) -> Result<Vec<(Vec<Value>, Document)>> {
        if live.is_empty() {
            return Ok(Vec::new());
        }
        let keys = live.iter().map(|d| d[pk].clone()).collect();
        let rows = self.execute(snap, &qsql::keyed_rows(schema, keys)?, &self.exec)?.rows;
        let mut by_key: HashMap<Part, Vec<Value>> = rows.into_iter().map(|r| (Part::of(&r[pk]), r)).collect();
        Ok(live.into_iter().filter_map(|d| by_key.remove(&Part::of(&d[pk])).map(|r| (r, d))).collect())
    }

    /// The row of table `table` with primary key `key`, read without SQL:
    /// the columns `SELECT * FROM table WHERE key = ...` answers, as it
    /// answers them ([`Db::get_columns`]). `None` when no live row has the key, or the row
    /// has expired ([`crate::schema::Schema::with_expire_by`]).
    pub fn get(&self, table: &str, key: &Value) -> Result<Option<Vec<Value>>> {
        Ok(self.get_many(table, std::slice::from_ref(key))?.pop().flatten())
    }

    /// [`Db::get`] of several keys, in their order.
    pub fn get_many(&self, table: &str, keys: &[Value]) -> Result<Vec<Option<Vec<Value>>>> {
        let snap = self.snapshot().table(table).ok_or_else(|| Error::Query(format!("no such table: {table}")))?;
        Ok(self.get_many_at(&snap, keys, &self.exec)?.into_iter().map(|r| r.map(|r| r.row)).collect())
    }

    /// [`Db::get_many`] on snapshot `snap`, of the table it looks at, with
    /// options (their `now` and `expired` decide which rows have
    /// expired), each row with the number of the write that last changed
    /// it. Each key is found by the table's primary-key indexes, newest
    /// segment first, and the memtable's newest rows as written, so that a
    /// read after each write builds nothing ([`Snapshot::find`]). An INT
    /// key may be given as text of its digits.
    pub fn get_many_at(&self, snap: &Snapshot, keys: &[Value], opts: &ExecOptions) -> Result<Vec<Option<KeyRow>>> {
        let fields: Vec<usize> = qsql::star_fields(snap.schema()).collect();
        self.get_fields_at(snap, keys, &fields, opts)
    }

    /// [`Db::get_many_at`] of the columns named `columns` alone, in that
    /// order: each one of those [`Db::get_columns`] lists. A read of a few
    /// columns of many rows (a ranker's features) leaves the rest unread:
    /// the doc store's blocks are not decompressed at all when every
    /// column asked for is a single-valued KEYWORD, INT or FLOAT, which
    /// their columns answer.
    pub fn get_columns_at(&self, snap: &Snapshot, keys: &[Value], columns: &[String], opts: &ExecOptions) -> Result<Vec<Option<KeyRow>>> {
        let fields = Self::key_read_fields(snap.schema(), columns)?;
        self.get_fields_at(snap, keys, &fields, opts)
    }

    /// The positions of the columns named `columns` (any case) of a table
    /// of schema `schema`, as a read by key answers them
    /// ([`Db::get_columns_at`]).
    pub fn key_read_fields(schema: &Schema, columns: &[String]) -> Result<Vec<usize>> {
        columns
            .iter()
            .map(|c| {
                schema.field_index(c).filter(|&i| qsql::star_fields(schema).any(|s| s == i)).ok_or_else(|| {
                    let names: Vec<String> = Self::get_columns(schema);
                    Error::Query(format!("{c:?} is not a column a read by key of {} answers; those are {}", schema.table, names.join(", ")))
                })
            })
            .collect()
    }

    fn get_fields_at(&self, snap: &Snapshot, keys: &[Value], fields: &[usize], opts: &ExecOptions) -> Result<Vec<Option<KeyRow>>> {
        let schema = snap.schema();
        let pk = schema.primary_key.ok_or_else(|| Error::Query(format!("{} has no primary key: rows are read by key only from a table with one", schema.table)))?;
        let keys: Vec<Value> = keys.iter().map(|k| key_value(schema, pk, k)).collect::<Result<_>>()?;
        let q = qsql::fields_select(schema, fields);
        let mut out = self.rows_by_key(snap, &keys, fields, opts)?;
        for row in out.iter_mut().flatten() {
            q.typed(std::slice::from_mut(&mut row.row));
        }
        Ok(out)
    }

    /// The live rows with primary keys `keys` (each a value of the key's
    /// type) on snapshot `snap`, of the table it looks at, in their order:
    /// each row its values of `fields` (positions) as [`Db::execute`] of
    /// a query of those fields gives them before typing them
    /// ([`Select::typed`]), and the number of the write that last changed
    /// it. `None` for a key no live row has, or whose row has expired at
    /// `opts`' time (unless `opts.expired`). Each key is found by the
    /// table's primary-key indexes, newest segment first, and the
    /// memtable's newest rows as written, so that a read after each write
    /// builds nothing ([`Snapshot::find`]).
    fn rows_by_key(&self, snap: &Snapshot, keys: &[Value], fields: &[usize], opts: &ExecOptions) -> Result<Vec<Option<KeyRow>>> {
        Ok(self.placed_rows_by_key(snap, keys, fields, opts)?.into_iter().map(|r| r.map(|(r, _)| r)).collect())
    }

    /// [`Db::rows_by_key`], each row with its place in the snapshot's
    /// segments (ordinal and docID, [`Snapshot::segments`]): the order a
    /// scan of them reads it in.
    fn placed_rows_by_key(&self, snap: &Snapshot, keys: &[Value], fields: &[usize], opts: &ExecOptions) -> Result<Vec<Option<(KeyRow, (usize, DocId))>>> {
        let schema = snap.schema();
        // The fields asked for, and then the time rows expire at when that
        // is not one of them.
        let mut fields = fields.to_vec();
        let shown = fields.len();
        let expiry = schema.expire_field().filter(|_| !opts.expired).map(|f| {
            let at = fields.iter().position(|&x| x == f).unwrap_or_else(|| {
                fields.push(f);
                fields.len() - 1
            });
            (at, opts.now.unwrap_or_else(crate::timestamp::now))
        });
        let numbered = snap.numbers_writes();
        let mut out: Vec<Option<(KeyRow, (usize, DocId))>> = vec![None; keys.len()];
        // Rows found in segments: (key's position, segment, docID, seq).
        let mut at = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            match snap.find(k) {
                None => {}
                Some(Found::Memtable { doc, seq, at }) => out[i] = Some((KeyRow { row: fields.iter().map(|&f| doc[f].clone()).collect(), seq_no: seq }, at)),
                Some(Found::Segment { seg, doc, view }) => {
                    let seq = if numbered { Snapshot::row_seq_of(view, doc)? } else { 0 };
                    at.push((i, seg, doc, seq));
                }
            }
        }
        if !at.is_empty() {
            // Read in the order of the segments' docIDs, whatever the keys'
            // order: the doc store keeps one decompressed block a segment,
            // so rows of the same block are then decompressed once rather
            // than once each.
            at.sort_unstable_by_key(|&(_, seg, doc, _)| (seg, doc));
            let q = qsql::fields_select(schema, &fields);
            let view = snap.found_view();
            let readers = self.readers(&view)?;
            let places: Vec<(usize, DocId)> = at.iter().map(|&(_, seg, doc, _)| (seg, doc)).collect();
            let rows = Executor::new(&view, &readers, opts).rows_at(&q, &places)?;
            for ((i, seg, doc, seq), row) in at.into_iter().zip(rows) {
                out[i] = Some((KeyRow { row, seq_no: seq }, (seg, doc)));
            }
        }
        for r in &mut out {
            let Some((row, _)) = r.as_mut() else { continue };
            if let Some((f, now)) = expiry {
                if matches!(row.row[f], Value::Int(t) | Value::Timestamp(t) if t <= now) {
                    *r = None;
                    continue;
                }
            }
            row.row.truncate(shown);
        }
        Ok(out)
    }

    /// [`Db::execute`] of `select` on `snap` without the executor, when
    /// its filter only pins the primary key to values (`WHERE id = ?`,
    /// `WHERE id IN (...)`, [`keys_read`]) and it reads plain columns: the
    /// rows are read by key ([`Db::rows_by_key`]) and answered in the
    /// order, and with the expiry, LIMIT, OFFSET, `max_rows` and typing,
    /// the executor gives them. The memtable's newest chunk, which every
    /// write changes, is then read as written rather than built as a
    /// segment, so that a write and a read of its row after it build
    /// nothing. `None` for any other query, and for one of more than
    /// `most` keys.
    fn by_key(&self, snap: &Snapshot, select: &Select, opts: &ExecOptions, most: usize) -> Result<Option<ResultSet>> {
        let Some((keys, fields)) = keys_read(snap.schema(), select, opts).filter(|(k, _)| k.len() <= most) else { return Ok(None) };
        crate::query::steps::epoch();
        let t0 = std::time::Instant::now();
        let reach = select.limit.unwrap_or(0).saturating_add(select.offset);
        if let Some(max) = opts.max_rows.filter(|&m| reach > m) {
            return Err(Error::Query(format!("LIMIT and OFFSET reach row {reach}; a query returns at most {max} rows")));
        }
        let placed = self.placed_rows_by_key(snap, &keys, &fields, opts)?;
        // In the order a scan of the segments reads the rows.
        let mut found: Vec<((usize, DocId), Value, Vec<Value>)> = keys.into_iter().zip(placed).filter_map(|(k, r)| r.map(|(r, at)| (at, k, r.row))).collect();
        found.sort_by_key(|(at, _, _)| *at);
        // The rows the filter matches, as the executor counts them.
        let matched = found.len() as u64;
        if let Some(want) = select.limit.map(|l| l.saturating_add(select.offset)) {
            found.truncate(want);
        }
        if let Some(max) = opts.max_rows.filter(|&m| select.limit.is_none() && found.len() > m) {
            return Err(Error::Query(format!("the query matches more than {max} rows, the most a query returns; add a LIMIT")));
        }
        // A shard folds OFFSET into LIMIT, and answers with the stored
        // integers, which the merge types.
        let skip = if opts.shard { 0 } else { select.offset };
        let (keys, mut rows): (Vec<Value>, Vec<Vec<Value>>) = found.into_iter().skip(skip).map(|(_, k, r)| (k, r)).unzip();
        if !opts.shard {
            select.typed(&mut rows);
        }
        let mut stats = ExecStats { filter_docs: matched, ..ExecStats::default() };
        let detail = format!("{} key{}", rows.len(), if rows.len() == 1 { "" } else { "s" });
        stats.plan.push(format!("read by key: {detail} found"));
        let step = crate::query::steps::Step { op: "read by key".into(), detail, start_us: crate::query::steps::us_at(t0), us: crate::query::steps::tenth(t0.elapsed().as_nanos() as f64 / 1e3), rows: Some(rows.len() as u64), ..Default::default() };
        stats.steps.push(step);
        let columns = select.projection.iter().map(|(n, _)| n.clone()).collect();
        let scores = vec![0.0; rows.len()];
        Ok(Some(ResultSet { columns, rows, keys, scores, parts: None, stats }))
    }

    /// The columns [`Db::get`] answers for a table of schema `schema`, in
    /// its rows' order: those `SELECT *` answers (the stored fields but
    /// vectors).
    pub fn get_columns(schema: &Schema) -> Vec<String> {
        qsql::star_fields(schema).map(|i| schema.fields[i].name.clone()).collect()
    }

    /// Deletes the rows of every table with `expire_by` that have expired
    /// at `now` (microseconds since the epoch), at most `batch` rows a
    /// transaction, by logged deletes: a replica applies them as it applies
    /// any, so that its rows stay its primary's. Queries and reads by key leave expired rows out
    /// before this deletes them; it frees their space. Returns how many it
    /// deleted. A primary runs it now and then (chloedb-server does, every
    /// few seconds); a replica, never.
    pub fn expire(&self, now: i64, batch: usize) -> Result<u64> {
        let batch = batch.max(1);
        let mut total = 0;
        let snap = self.snapshot();
        let tables: Vec<(String, usize, usize)> = snap.schemas().filter_map(|s| Some((s.table.clone(), s.expire_field()?, s.primary_key?))).collect();
        for (table, f, pk) in tables {
            let opts = ExecOptions { expired: true, max_rows: None, ..self.exec.clone() };
            loop {
                let snap = self.snapshot().table(&table).ok_or_else(|| Error::Query(format!("no such table: {table}")))?;
                let schema = snap.schema();
                if schema.expire_field() != Some(f) || schema.primary_key != Some(pk) {
                    break;
                }
                let q = Select {
                    projection: vec![(schema.fields[pk].name.clone(), crate::query::logical::Projection::Field(pk))],
                    filter: crate::query::logical::Pred::IntRange { field: f, lo: i64::MIN, hi: now },
                    order: None,
                    group_by: Vec::new(),
                    paths: Vec::new(),
                    times: Vec::new(),
                    having: None,
                    tiebreak: None,
                    phases: Vec::new(),
                    limit: Some(batch),
                    offset: 0,
                    graphs: Vec::new(),
                    types: vec![None],
                    guard: None,
                    per: None,
                };
                let keys: Vec<Value> = self.execute(&snap, &q, &opts)?.rows.into_iter().filter_map(|mut r| r.pop()).collect();
                if keys.is_empty() {
                    break;
                }
                let mut txn = self.storage.begin()?;
                let n = txn.delete_expired(&table, &keys, now)?;
                txn.commit()?;
                total += n as u64;
                if keys.len() < batch || n == 0 {
                    break;
                }
            }
        }
        Ok(total)
    }

    /// `opts` for a query of the table `snap` looks at: with the table's
    /// own synonym list after the server's, when it has one
    /// ([`crate::query::table_synonyms`]).
    fn table_options<'a>(&self, snap: &Snapshot, opts: &'a ExecOptions) -> Result<std::borrow::Cow<'a, ExecOptions>> {
        let table = &snap.shared_schema().table;
        // The system tables have none, and reading the functions reads one.
        if table.starts_with('_') {
            return Ok(std::borrow::Cow::Borrowed(opts));
        }
        let functions = self.functions_at(snap)?;
        Ok(match crate::query::table_synonyms::for_table(&functions, table, opts.synonyms.as_ref())? {
            Some(s) => std::borrow::Cow::Owned(ExecOptions { synonyms: Some(s), ..opts.clone() }),
            None => std::borrow::Cow::Borrowed(opts),
        })
    }

    /// The answer of query `select` on snapshot `snap`, of the table it
    /// looks at, with options `opts`: by the executor, or by key when its
    /// filter only names keys ([`Db::by_key`]).
    pub fn execute(&self, snap: &Snapshot, select: &Select, opts: &ExecOptions) -> Result<ResultSet> {
        if let Some(rs) = self.by_key(snap, select, opts, usize::MAX)? {
            return Ok(rs);
        }
        // Fixed before the first query's first step begins.
        crate::query::steps::epoch();
        let t0 = std::time::Instant::now();
        let readers = self.readers(snap)?;
        let opened = t0.elapsed();
        let opts = self.table_options(snap, opts)?;
        let mut rs = Executor::new(snap, &readers, &opts).execute(select)?;
        opened_step(&mut rs.stats, t0, opened, readers.len());
        Ok(rs)
    }

    /// Starts a scroll of `select` on `snap` ([`Scroll`]): its first page,
    /// of at most `page` rows, and the scroll that reads the rest a page at
    /// a time ([`Db::scroll_next`]) from the same snapshot, which the
    /// caller keeps until the last. [`ExecOptions::max_rows`] does not
    /// apply: `page` caps each page, and a query other than a plain scan
    /// (no ORDER BY, groups or counts) holds at most `hold` rows' places,
    /// or rows, between pages.
    pub fn scroll(&self, snap: &Snapshot, select: &Select, opts: &ExecOptions, page: usize, hold: usize) -> Result<(ResultSet, Scroll)> {
        let opts = ExecOptions { max_rows: None, ..opts.clone() };
        if let Some(rs) = self.by_key(snap, select, &opts, usize::MAX)? {
            return Ok(Scroll::of_rows(rs, page));
        }
        crate::query::steps::epoch();
        let t0 = std::time::Instant::now();
        let readers = self.readers(snap)?;
        let opened = t0.elapsed();
        let opts = self.table_options(snap, &opts)?;
        let (mut rs, s) = Executor::new(snap, &readers, &opts).scroll(select, page, hold)?;
        opened_step(&mut rs.stats, t0, opened, readers.len());
        Ok((rs, s))
    }

    /// The next page of `scroll`, of at most `page` rows, from `snap`, the
    /// snapshot it started on: empty once every row has been read
    /// ([`Scroll::done`]). A page that fails leaves the scroll where it was.
    pub fn scroll_next(&self, snap: &Snapshot, scroll: &mut Scroll, opts: &ExecOptions, page: usize) -> Result<ResultSet> {
        if !scroll.reads() {
            return Ok(scroll.next_rows(page));
        }
        let opts = ExecOptions { max_rows: None, ..opts.clone() };
        crate::query::steps::epoch();
        let t0 = std::time::Instant::now();
        let readers = self.readers(snap)?;
        let opened = t0.elapsed();
        let opts = self.table_options(snap, &opts)?;
        let mut rs = Executor::new(snap, &readers, &opts).scroll_next(scroll, page)?;
        opened_step(&mut rs.stats, t0, opened, readers.len());
        Ok(rs)
    }

    /// [`Db::execute`] of a compound query's leaf (without the feature
    /// `datafusion`, its rows).
    #[cfg(not(feature = "datafusion"))]
    pub fn execute_leaf(&self, snap: &Snapshot, select: &Select, opts: &ExecOptions, _over: usize) -> Result<Leaf> {
        self.execute(snap, select, opts).map(Leaf::Rows)
    }

    /// [`Db::execute`] for DataFusion: the rows as Arrow columns when there
    /// are more than `over` ([`Executor::execute_arrow`]).
    #[cfg(feature = "datafusion")]
    pub fn execute_leaf(&self, snap: &Snapshot, select: &Select, opts: &ExecOptions, over: usize) -> Result<Leaf> {
        // By key when it cannot answer more than `over` rows: a leaf of more
        // (a join's side restricted to the other's keys) is read as
        // columns.
        if let Some(rs) = self.by_key(snap, select, opts, over)? {
            return Ok(Leaf::Rows(rs));
        }
        crate::query::steps::epoch();
        let t0 = std::time::Instant::now();
        let readers = self.readers(snap)?;
        let opened = t0.elapsed();
        let opts = self.table_options(snap, opts)?;
        let mut leaf = Executor::new(snap, &readers, &opts).execute_arrow(select, over)?;
        let stats = match &mut leaf {
            Leaf::Rows(rs) => &mut rs.stats,
            Leaf::Arrow(c) => &mut c.stats,
        };
        opened_step(stats, t0, opened, readers.len());
        Ok(leaf)
    }

    /// This database's BM25 statistics for the terms `select` scores with,
    /// for a coordinator to sum over shards and pass back through
    /// [`ExecOptions::stats`] ([`crate::cluster`]).
    pub fn term_stats(&self, snap: &Snapshot, select: &Select) -> Result<TermStats> {
        self.term_stats_with(snap, select, &self.exec)
    }

    /// [`Db::term_stats`] with options (their synonyms widen the terms).
    pub fn term_stats_with(&self, snap: &Snapshot, select: &Select, opts: &ExecOptions) -> Result<TermStats> {
        let readers = self.readers(snap)?;
        Executor::new(snap, &readers, &*self.table_options(snap, opts)?).term_stats(select)
    }

    /// The words of `text` that look like typos of the terms of TEXT field
    /// `field` of `table`, and the text with each replaced by the word it
    /// most likely meant: "did you mean" ([`Executor::corrections`]).
    pub fn correct(&self, table: &str, field: &str, text: &str) -> Result<Corrected> {
        let all = self.snapshot();
        let snap = all.table(table).ok_or_else(|| Error::Query(format!("no table {table}")))?;
        let schema = snap.shared_schema().clone();
        let f = schema.field_index(field).ok_or_else(|| Error::Query(format!("{table} has no field {field}")))?;
        if !matches!(schema.fields[f].ty, FieldType::Text { .. }) {
            return Err(Error::Query(format!("{table}.{field} is not a TEXT field: only text is corrected")));
        }
        let readers = self.readers(&snap)?;
        let words = Executor::new(&snap, &readers, &self.exec).corrections(f, text)?;
        Ok(Corrected { text: crate::query::exec::corrected_text(text, &words), changed: !words.is_empty(), words })
    }

    /// The loosest level of `select`'s guard (`min_rows`, `max_rows`) and
    /// each level's rows on snapshot `snap`, from it to the strictest
    /// ([`Executor::guard_counts`]): what a coordinator sums over its
    /// shards to choose the level ([`crate::query::logical::Guard::level`]).
    pub fn guard_counts(&self, snap: &Snapshot, select: &Select, opts: &ExecOptions) -> Result<(usize, Vec<u64>)> {
        if select.guard.is_none() {
            return Err(Error::Query("the query has no guard (min_rows, max_rows) to count the levels of".into()));
        }
        let readers = self.readers(snap)?;
        Executor::new(snap, &readers, &*self.table_options(snap, opts)?).guard_counts(select)
    }

    /// The codes that can follow each of `prefixes` in semantic-ID field
    /// `field` of table `table`, among the rows that SQL condition `filter`
    /// (with `params`) matches, or all rows: per prefix, each code one
    /// number longer that such rows have, with how many rows have it, in
    /// the order of their last numbers. The prefix `""` asks for the first
    /// numbers. At most [`crate::sid::MAX_PREFIXES`] prefixes.
    pub fn sid_next(&self, table: &str, field: &str, prefixes: &[String], filter: Option<&str>, params: &Params) -> Result<Vec<Vec<(String, u64)>>> {
        let snap = self.snapshot().table(table).ok_or_else(|| Error::Query(format!("no such table: {table}")))?;
        let select = qsql::bind_condition(snap.schema(), filter, params)?;
        self.sid_next_with(&snap, &select, field, prefixes, &self.exec)
    }

    /// [`Db::sid_next`] on snapshot `snap`, among the rows `select`'s
    /// filter matches (the rest of it is not used), with options.
    pub fn sid_next_with(&self, snap: &Snapshot, select: &Select, field: &str, prefixes: &[String], opts: &ExecOptions) -> Result<Vec<Vec<(String, u64)>>> {
        let schema = snap.schema();
        let f = schema.field_index(field).ok_or_else(|| Error::Query(format!("no field {field} in {}", schema.table)))?;
        if schema.fields[f].semantic_id.is_none() {
            return Err(Error::Query(format!("{field} is not a semantic-ID field")));
        }
        if prefixes.is_empty() || prefixes.len() > crate::sid::MAX_PREFIXES {
            return Err(Error::Query(format!("ask for the next codes of 1 to {} prefixes, not {}", crate::sid::MAX_PREFIXES, prefixes.len())));
        }
        if let Some(p) = prefixes.iter().find(|p| !p.is_empty() && crate::sid::parse(p).is_none_or(|c| c.len() > crate::sid::MAX_LEVELS)) {
            return Err(Error::Query(format!("{p:?} is not a prefix of a semantic ID: numbers joined by dots, such as 12.7, or \"\" for the first")));
        }
        let readers = self.readers(snap)?;
        Executor::new(snap, &readers, opts).sid_next(select, f, prefixes)
    }

    /// The paths listing of a JSON field that `call` asks for
    /// ([`crate::query::paths`]): each path its objects hold, with its
    /// rows, types and values, read from the field's terms. `SELECT * FROM
    /// json_paths('table', 'field')` runs it as a query.
    pub fn json_paths(&self, call: &crate::query::paths::Call, params: &Params) -> Result<crate::query::paths::Paths> {
        self.json_paths_at(&self.snapshot(), call, params, &self.exec)
    }

    /// [`Db::json_paths`] on snapshot `snap`, with options.
    pub fn json_paths_at(&self, snap: &Snapshot, call: &crate::query::paths::Call, params: &Params, opts: &ExecOptions) -> Result<crate::query::paths::Paths> {
        let snap = snap.table(&call.table).ok_or_else(|| Error::Query(format!("no such table: {}", call.table)))?;
        let schema = snap.schema();
        let f = call.field_of(schema)?;
        let select = qsql::bind_condition(schema, call.filter.as_deref(), params)?;
        let readers = self.readers(&snap)?;
        Executor::new(&snap, &readers, opts).json_paths(&select, f, call)
    }

    /// Builds a segment image of `docs` for this schema into `dest` (a new
    /// file), to attach to a database elsewhere ([`Db::attach_segments`]).
    pub fn write_segment(schema: &Schema, docs: Vec<Document>, opts: &crate::segment::build::BuildOptions, dest: &Path) -> Result<()> {
        let docs: Vec<Document> = docs
            .into_iter()
            .map(|mut d| {
                crate::value::conform(schema, &mut d)?;
                schema.fill_generated(&mut d);
                Ok(d)
            })
            .collect::<Result<_>>()?;
        let image = crate::segment::build::build_segment(schema, &docs, opts)?;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(dest)?;
        std::io::Write::write_all(&mut f, &image)?;
        f.sync_data()?;
        Ok(())
    }

    /// Attaches segment images built elsewhere, in one commit, after
    /// checking that every field of this table opens in each of them.
    pub fn attach_segments(&self, images: &[std::path::PathBuf]) -> Result<Vec<u64>> {
        let schema = self.schema().clone();
        for p in images {
            let file = SegmentFile::open(crate::postings::bytes::OwnedBytes::new(std::fs::read(p)?))?;
            file.verify_sections().map_err(|e| crate::Error::Schema(format!("{}: {e}", p.display())))?;
            let r = SegmentReaders::open(&file, &schema)?;
            for (i, f) in schema.fields.iter().enumerate() {
                let ok = match f.ty {
                    FieldType::Text { .. } | FieldType::Sparse => r.text(i).map(|_| ()),
                    FieldType::Json if f.has_text() => r.keyword(i).and_then(|_| r.text(i)).map(|_| ()),
                    FieldType::Keyword | FieldType::Json => r.keyword(i).map(|_| ()),
                    FieldType::Vector { .. } => r.vector(i).map(|_| ()),
                    FieldType::Edges => r.graph(i).map(|_| ()),
                    FieldType::Int | FieldType::Float => Ok(()),
                };
                ok.map_err(|e| crate::Error::Schema(format!("{}: field {}: {e}", p.display(), f.name)))?;
            }
            r.store()?;
        }
        self.storage.attach_segments(images)
    }

    /// Readers for every segment of `snap`, opened on first use.
    pub fn readers(&self, snap: &Snapshot) -> Result<Vec<Arc<SegmentReaders>>> {
        let at = |v: &crate::storage::SegmentView| v.file.image().as_ptr() as usize;
        // Readers are opened for one schema: an ALTER TABLE gives the
        // table's segments new ones. The cache holds the schema it opened
        // them with, so its address is not reused while an entry names it.
        let schema = snap.shared_schema();
        let fits = |(p, s, _): &&(usize, Arc<Schema>, Arc<SegmentReaders>), v: &crate::storage::SegmentView| *p == at(v) && Arc::ptr_eq(s, schema);
        let gen = self.storage.image_generation();
        let swapped = gen != self.readers_gen.load(std::sync::atomic::Ordering::Acquire);
        if !swapped {
            let cache = self.readers.read().unwrap_or_else(|e| e.into_inner());
            let hit = |v: &crate::storage::SegmentView| cache.get(&v.id).filter(|e| fits(e, v)).map(|(_, _, r)| r.clone());
            if let Some(all) = snap.segments().iter().map(hit).collect() {
                return Ok(all);
            }
        }
        let mut cache = self.readers.write().unwrap_or_else(|e| e.into_inner());
        if swapped {
            let live: HashMap<u64, usize> = snap.all_segments().map(|v| (v.id, at(v))).collect();
            cache.retain(|id, (p, _, _)| live.get(id) == Some(p));
            self.readers_gen.store(gen, std::sync::atomic::Ordering::Release);
        }
        let mut out = Vec::with_capacity(snap.segments().len());
        for v in snap.segments() {
            let r = match cache.get(&v.id).filter(|e| fits(e, v)) {
                Some((_, _, r)) => r.clone(),
                None => {
                    let r = Arc::new(SegmentReaders::open(&v.file, schema)?);
                    cache.insert(v.id, (at(v), schema.clone(), r.clone()));
                    r
                }
            };
            out.push(r);
        }
        // Segment ids are unique across tables; keep the readers of every
        // table, so that queries on different tables do not evict each other's.
        if cache.len() > 2 * snap.all_segments().count() + 16 {
            let live: HashSet<u64> = snap.all_segments().map(|v| v.id).collect();
            cache.retain(|id, _| live.contains(id));
        }
        Ok(out)
    }

    /// Drops cached readers (e.g. before cold-cache measurements).
    pub fn clear_readers(&self) {
        self.readers.write().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Documents [`Db::load`] embeds at a time.
const EMBED_BATCH: usize = 4096;

/// Documents [`Db::load`] embeds at a time into a table with a field of
/// images' vectors: their files are held until they are embedded.
const MEDIA_BATCH: usize = 64;

/// The bytes of the files a write's MEDIA values named, by hash: read
/// once when the values were resolved, for the vectors embedded from them.
type Held = HashMap<String, Arc<[u8]>>;

/// Where the images an embedded field embeds come from: the bytes a write
/// held, else where the value says ([`Resolver::bytes_of`]).
struct Files<'a> {
    resolver: &'a Resolver,
    held: Held,
}

impl Files<'_> {
    fn bytes(&self, m: &Media) -> Result<Arc<[u8]>> {
        match self.held.get(&m.sha256) {
            Some(b) => Ok(b.clone()),
            None => Ok(self.resolver.bytes_of(m)?.into()),
        }
    }
}

/// Whether field `g` is a source of a generated field among `from`.
fn generated_from_set(schema: &Schema, from: &[String], g: usize) -> bool {
    from.iter().filter_map(|s| schema.field(s)).any(|f| f.generated_from.iter().any(|x| schema.field_index(x) == Some(g)))
}

/// [`Db::resolve_media_in`] with resolver `r`, for documents of `schema`.
/// Returns the bytes read of the files a field embeds.
fn resolve_media(r: &Resolver, schema: &Schema, docs: &mut [Document]) -> Result<Held> {
    let mut held = Held::new();
    for (f, field) in schema.fields.iter().enumerate() {
        let Some(kind) = field.media else { continue };
        let at: Vec<usize> = (0..docs.len()).filter(|&j| matches!(docs[j][f], Value::Str(_))).collect();
        let resolved = {
            let inputs: Vec<&str> = at.iter().filter_map(|&j| docs[j][f].as_str()).collect();
            r.resolve_all(kind, &inputs)
        };
        let read = read_by_model(schema, f);
        for (j, m) in at.into_iter().zip(resolved) {
            let m = m.map_err(|e| media_error(&field.name, e))?;
            if let Some(data) = m.data.filter(|_| read) {
                held.insert(m.media.sha256.clone(), data);
            }
            docs[j][f] = Value::Str(m.media.to_text());
        }
    }
    Ok(held)
}

/// Resolves the MEDIA values an UPDATE's `SET` gives as constants.
fn resolve_sets(r: &Resolver, schema: &Schema, sets: &mut [(usize, Assign)]) -> Result<Held> {
    let mut held = Held::new();
    for (f, a) in sets {
        let (Some(kind), Assign::Value(Value::Str(t))) = (schema.fields[*f].media, &*a) else { continue };
        let m = r.resolve(kind, t).map_err(|e| media_error(&schema.fields[*f].name, e))?;
        if let Some(data) = m.data.filter(|_| read_by_model(schema, *f)) {
            held.insert(m.media.sha256.clone(), data);
        }
        *a = Assign::Value(Value::Str(m.media.to_text()));
    }
    Ok(held)
}

/// Whether a model reads the files of MEDIA field `f`: a field embeds it,
/// or is derived from it.
fn read_by_model(schema: &Schema, f: usize) -> bool {
    (0..schema.fields.len()).any(|g| schema.embeds_media(g) == Some(f) || schema.derives_from(g) == Some(f))
}

fn media_error(field: &str, e: Error) -> Error {
    match e {
        Error::Schema(m) => Error::Schema(format!("field {field}: {m}")),
        Error::Timeout(m) => Error::Timeout(format!("field {field}: {m}")),
        e => e,
    }
}

/// Computes the embedded vectors `docs` leave out (null) with `models`
/// (by field of `schema`), one batch per field split across the cores. A
/// document whose sources are all empty gets no vector.
fn embed_with(models: &[Option<Arc<dyn Embedder>>], schema: &Schema, docs: &mut [Document], files: &Files) -> Result<()> {
    embed_known(models, schema, docs, &Known::new(), files)
}

/// [`embed_with`], taking the vectors of the texts `known` has from it.
fn embed_known(models: &[Option<Arc<dyn Embedder>>], schema: &Schema, docs: &mut [Document], known: &Known, files: &Files) -> Result<()> {
    for (f, e) in models.iter().enumerate() {
        let Some(e) = e else { continue };
        let ahead = known.get(&f);
        let mut todo = Vec::new();
        let mut texts = Vec::new();
        for (j, d) in docs.iter_mut().enumerate() {
            if matches!(d[f], Value::Null) {
                let t = schema.embed_text(f, d);
                if let Some(v) = ahead.and_then(|a| a.get(&t)) {
                    d[f] = v.clone();
                } else if !t.is_empty() {
                    todo.push(j);
                    texts.push(t);
                }
            }
        }
        for (j, v) in todo.into_iter().zip(embedded_values(e.as_ref(), schema, f, &texts, files)?) {
            docs[j][f] = v;
        }
    }
    Ok(())
}

/// The vectors `docs` leave out (null) of their embedded fields, each text
/// embedded once.
fn vectors_of(models: &[Option<Arc<dyn Embedder>>], schema: &Schema, docs: &[Document], files: &Files) -> Result<Known> {
    let mut known = Known::new();
    for (f, e) in models.iter().enumerate() {
        let Some(e) = e else { continue };
        let mut seen = HashSet::new();
        let texts: Vec<String> = docs.iter().filter(|d| matches!(d[f], Value::Null)).map(|d| schema.embed_text(f, d)).filter(|t| !t.is_empty() && seen.insert(t.clone())).collect();
        if !texts.is_empty() {
            let values = embedded_values(e.as_ref(), schema, f, &texts, files)?;
            known.insert(f, texts.into_iter().zip(values).collect());
        }
    }
    Ok(known)
}

/// The values of embedded field `f` of `schema` for `sources` (as
/// [`Schema::embed_text`] gives them): a VECTOR field's vectors
/// ([`embed_sources`]), or a SPARSE field's term weights, as the JSON
/// object a SPARSE value is ([`Embedder::embed_sparse`]).
fn embedded_values(e: &dyn Embedder, schema: &Schema, f: usize, sources: &[String], files: &Files) -> Result<Vec<Value>> {
    if schema.fields[f].ty == FieldType::Sparse {
        let refs: Vec<&str> = sources.iter().map(String::as_str).collect();
        let terms = e.embed_sparse(&refs)?;
        if terms.len() != sources.len() {
            return Err(Error::Schema(format!("model {} made the terms of {} texts of {}", e.name(), terms.len(), sources.len())));
        }
        return Ok(terms.iter().map(|t| Value::Str(crate::embed::terms_json(t))).collect());
    }
    Ok(embed_sources(e, schema, f, sources, files)?.into_iter().map(|v| embedded_value(schema, f, v.into())).collect())
}

/// The value of embedded field `f` of `schema` from what [`embed_sources`]
/// made: the vector, or the vectors of a multi-vector field one after
/// another (none: no value).
fn embedded_value(schema: &Schema, f: usize, v: Arc<[f32]>) -> Value {
    match &schema.fields[f] {
        field if field.multi => {
            let FieldType::Vector { dim, .. } = field.ty else { return Value::Null };
            match v.is_empty() || dim == 0 {
                true => Value::Null,
                false => Value::List(v.chunks(dim).map(|c| Value::Vector(c.into())).collect()),
            }
        }
        _ => Value::Vector(v),
    }
}

/// The vectors of field `f` of `schema` for `sources` (as
/// [`Schema::embed_text`] gives them): of the texts, or of the images or
/// sounds that the MEDIA values name, decoded a batch at a time. A
/// multi-vector field's are the vectors of each text's tokens, one after
/// another ([`Embedder::embed_tokens`]).
fn embed_sources(e: &dyn Embedder, schema: &Schema, f: usize, sources: &[String], files: &Files) -> Result<Vec<Vec<f32>>> {
    if schema.fields[f].multi {
        return embed_all_tokens(e, sources);
    }
    let Some(m) = schema.embeds_media(f) else { return embed_all(e, sources) };
    if schema.fields[m].media == Some(crate::media::MediaKind::Audio) {
        return in_sound_batches(&schema.fields[m].name, sources, files, |inputs| e.embed_inputs(inputs));
    }
    let mut out = Vec::with_capacity(sources.len());
    for batch in sources.chunks(16) {
        let images = images_of(&schema.fields[m].name, batch, files)?;
        let inputs: Vec<Input> = images.iter().map(Input::Image).collect();
        out.extend(e.embed_inputs(&inputs)?);
    }
    Ok(out)
}

/// The images MEDIA values of field `field` name, decoded.
fn images_of(field: &str, values: &[String], files: &Files) -> Result<Vec<crate::embed::Image>> {
    values
        .iter()
        .map(|t| {
            let m = Media::parse(t).map_err(Error::Schema)?;
            let data = files.bytes(&m)?;
            crate::media::decode_image(&data).map_err(|e| Error::Schema(format!("{}: {e}", m.uri)))
        })
        .collect::<Result<Vec<_>>>()
        .map_err(|e| media_error(field, e))
}

/// Makes the derived text `docs` leave out (null) with `derivers` (by
/// field of `schema`), taking it from `known` where it has the file's, and
/// fills in again the generated fields of the documents that got text. A
/// document without a file gets none.
fn derive_known(derivers: &[Option<Arc<dyn Deriver>>], schema: &Schema, docs: &mut [Document], known: &Derived, files: &Files) -> Result<()> {
    let mut changed = vec![false; docs.len()];
    for (f, d) in derivers.iter().enumerate() {
        let (Some(d), Some(m)) = (d, schema.derives_from(f)) else { continue };
        let ahead = known.get(&f);
        let mut todo = Vec::new();
        let mut values = Vec::new();
        for (j, doc) in docs.iter_mut().enumerate() {
            let Value::Str(v) = &doc[m] else { continue };
            if !matches!(doc[f], Value::Null) {
                continue;
            }
            changed[j] = true;
            match ahead.and_then(|a| a.get(v)) {
                Some(t) => doc[f] = derived_value(&schema.fields[f], t),
                None => {
                    todo.push(j);
                    values.push(v.clone());
                }
            }
        }
        for (j, t) in todo.into_iter().zip(derive_sources(d.as_ref(), schema, m, &values, files)?) {
            docs[j][f] = derived_value(&schema.fields[f], &t);
        }
    }
    if schema.fields.iter().any(|f| !f.generated_from.is_empty()) {
        docs.iter_mut().zip(changed).filter(|(_, c)| *c).for_each(|(d, _)| schema.fill_generated(d));
    }
    Ok(())
}

/// A derived field's value of the text its model made: a multi-valued
/// field's values one a line ([`crate::content::SEP`]).
fn derived_value(f: &crate::schema::Field, t: &str) -> Value {
    if f.multi {
        let vs: Vec<Value> = t.split(crate::content::SEP).filter(|v| !v.is_empty()).map(|v| Value::Str(v.to_string())).collect();
        return if vs.is_empty() { Value::Null } else { Value::List(vs) };
    }
    // A keyword a model made none of (a classifier not trained yet).
    if f.ty == crate::schema::FieldType::Keyword && t.is_empty() {
        return Value::Null;
    }
    Value::Str(t.to_string())
}

/// The text `docs` leave out (null) of their derived fields, each file
/// read once.
fn derived_of(derivers: &[Option<Arc<dyn Deriver>>], schema: &Schema, docs: &[Document], files: &Files) -> Result<Derived> {
    let mut known = Derived::new();
    for (f, d) in derivers.iter().enumerate() {
        let (Some(d), Some(m)) = (d, schema.derives_from(f)) else { continue };
        let mut seen = HashSet::new();
        let values: Vec<String> = docs.iter().filter(|doc| matches!(doc[f], Value::Null)).filter_map(|doc| doc[m].as_str()).filter(|v| seen.insert(v.to_string())).map(String::from).collect();
        if !values.is_empty() {
            let texts = derive_sources(d.as_ref(), schema, m, &values, files)?;
            known.insert(f, values.into_iter().zip(texts).collect());
        }
    }
    Ok(known)
}

/// The text `d` makes of the files that `values` of MEDIA field `m` name,
/// decoded a batch at a time: 16 images, or up to 4 sounds of 2 minutes
/// together (a sound can be an hour long).
fn derive_sources(d: &dyn Deriver, schema: &Schema, m: usize, values: &[String], files: &Files) -> Result<Vec<String>> {
    let field = &schema.fields[m].name;
    // A TEXT or KEYWORD field's values are the inputs themselves, each
    // distinct one given once (a language model's request each).
    if schema.fields[m].media.is_none() {
        let mut at: HashMap<&str, usize> = HashMap::new();
        let mut distinct: Vec<&str> = Vec::new();
        let slots: Vec<usize> = values
            .iter()
            .map(|v| {
                *at.entry(v.as_str()).or_insert_with(|| {
                    distinct.push(v.as_str());
                    distinct.len() - 1
                })
            })
            .collect();
        let mut made = Vec::with_capacity(distinct.len());
        for batch in distinct.chunks(32) {
            let inputs: Vec<Input> = batch.iter().map(|v| Input::Text(v)).collect();
            made.extend(d.derive(&inputs)?);
        }
        return Ok(slots.into_iter().map(|i| made[i].clone()).collect());
    }
    if d.modality() == Modality::Audio {
        return in_sound_batches(field, values, files, |inputs| d.derive(inputs));
    }
    let mut out = Vec::with_capacity(values.len());
    for batch in values.chunks(16) {
        let images = images_of(field, batch, files)?;
        let inputs: Vec<Input> = images.iter().map(Input::Image).collect();
        out.extend(d.derive(&inputs)?);
    }
    Ok(out)
}

/// What `f` makes of the sounds that MEDIA values of field `field` name,
/// decoded and given to it up to 4 at a time, or 2 minutes of sound
/// together (a sound can be an hour long).
fn in_sound_batches<T>(field: &str, values: &[String], files: &Files, mut f: impl FnMut(&[Input<'_>]) -> Result<Vec<T>>) -> Result<Vec<T>> {
    let mut out = Vec::with_capacity(values.len());
    let mut batch: Vec<crate::embed::Audio> = Vec::new();
    for (i, v) in values.iter().enumerate() {
        batch.extend(sounds_of(field, std::slice::from_ref(v), files)?);
        let seconds: f64 = batch.iter().map(|a| a.samples.len() as f64 / a.rate.max(1) as f64).sum();
        if batch.len() == 4 || seconds >= 120.0 || i + 1 == values.len() {
            out.extend(f(&batch.iter().map(Input::Audio).collect::<Vec<_>>())?);
            batch.clear();
        }
    }
    Ok(out)
}

/// The sounds MEDIA values of field `field` name, decoded.
fn sounds_of(field: &str, values: &[String], files: &Files) -> Result<Vec<crate::embed::Audio>> {
    values
        .iter()
        .map(|t| {
            let m = Media::parse(t).map_err(Error::Schema)?;
            let data = files.bytes(&m)?;
            crate::media::decode_audio(&data).map_err(|e| Error::Schema(format!("{}: {e}", m.uri)))
        })
        .collect::<Result<Vec<_>>>()
        .map_err(|e| media_error(field, e))
}

/// What setting fields of live rows writes ([`plan_sets`]).
struct Planned {
    /// Key, field and value of each number set in place.
    in_place: Vec<(Value, usize, Value)>,
    /// The rows rewritten, the vectors their new sources make stale left
    /// out (null).
    rewrites: Vec<Document>,
    rows: u64,
}

/// Plans setting fields of live rows: each pair is a row (every field, as
/// a SELECT of them all returns it) and, for `ON CONFLICT DO UPDATE`, the
/// document the INSERT proposed ([`Assign::Excluded`]). Only single-valued
/// numbers change in place; any other field rewrites the row, which needs
/// every text field stored or generated.
fn plan_sets(schema: &Schema, models: &[Option<Arc<dyn Embedder>>], derivers: &[Option<Arc<dyn Deriver>>], rows: Vec<(Vec<Value>, Document)>, sets: &[(usize, Assign)]) -> Result<Planned> {
    let pk = schema.primary_key.ok_or_else(|| Error::Query(format!("UPDATE needs a primary key, and {} has none", schema.table)))?;
    if let Some(f) = sets.iter().map(|(f, _)| &schema.fields[*f]).find(|f| f.semantic_id.is_some()) {
        let from = &f.semantic_id.as_ref().expect("a semantic-ID field").from;
        return Err(Error::Query(format!("{} is a semantic ID: the writer derives it from {from}; set {from} instead", f.name)));
    }
    if let Some(f) = schema.shard_by_field().filter(|f| sets.iter().any(|(g, _)| g == f)) {
        return Err(Error::Query(format!("{} places {}'s rows on shards (shard_by), so UPDATE does not change it: delete the row and insert it again", schema.fields[f].name, schema.table)));
    }
    let rewrite = !in_place(schema, sets);
    if rewrite {
        if let Some(f) = schema.fields.iter().find(|f| matches!(f.ty, FieldType::Text { .. }) && !f.stored && f.generated_from.is_empty()) {
            return Err(Error::Query(format!("UPDATE of non-numeric fields rewrites documents, and {} is not stored", f.name)));
        }
    }
    let (remade, stale) = cleared(schema, models, derivers, sets);
    // Expressions read generated fields, which need not be stored.
    let generated = schema.fields.iter().any(|f| !f.generated_from.is_empty()) && sets.iter().any(|(_, a)| matches!(a, Assign::Expr(_)));
    let (mut in_place, mut rewrites, mut n) = (Vec::new(), Vec::new(), 0u64);
    for (mut row, proposed) in rows {
        if generated {
            schema.fill_generated(&mut row);
        }
        // The row as an expression reads it: every right-hand side sees
        // the row as it was. A multi-valued field without values is an
        // empty list, as queries compare it.
        let read = |r: Ref| -> Result<Value> {
            let (doc, g) = match r {
                Ref::Field(g) => (&row, g as usize),
                Ref::Excluded(g) => (&proposed, g as usize),
                r => return Err(Error::Query(format!("SET reads fields of the row, not {r:?}"))),
            };
            let field = schema.fields.get(g).ok_or_else(|| Error::Query(format!("{} has no field #{g}", schema.table)))?;
            Ok(match doc.get(g).cloned().unwrap_or(Value::Null) {
                Value::Null if field.multi => Value::List(Vec::new()),
                v => field.output(v),
            })
        };
        let new = |f: usize, a: &Assign| -> Result<Value> {
            Ok(match a {
                Assign::Value(v) => v.clone(),
                Assign::Excluded(g) => proposed[*g].clone(),
                Assign::Expr(e) => qsql::assigned(&schema.fields[f], eval::eval(e, &read)?)?,
                // NULL counts as zero; a sum that overflows is an
                // error, as in Postgres.
                Assign::Add(x) => {
                    let overflow = || Error::Query(format!("{} + {x:?} overflows", schema.fields[f].name));
                    match (&schema.fields[f].ty, x) {
                        (FieldType::Int, Value::Int(x)) => Value::Int(row[f].as_i64().unwrap_or(0).checked_add(*x).ok_or_else(overflow)?),
                        (FieldType::Int, Value::Float(y)) if y.fract() == 0.0 && y.abs() < 9.2e18 => {
                            Value::Int(row[f].as_i64().unwrap_or(0).checked_add(*y as i64).ok_or_else(overflow)?)
                        }
                        (FieldType::Int, _) => return Err(Error::Query(format!("{} is INT: add an integer", schema.fields[f].name))),
                        _ => {
                            let y = row[f].as_f64().unwrap_or(0.0) + x.as_f64().unwrap_or(0.0);
                            if !y.is_finite() {
                                return Err(overflow());
                            }
                            Value::Float(y)
                        }
                    }
                }
            })
        };
        if !rewrite {
            for (f, a) in sets {
                in_place.push((row[pk].clone(), *f, new(*f, a)?));
            }
        } else {
            let mut doc = row.clone();
            for (f, a) in sets {
                doc[*f] = new(*f, a)?;
            }
            for &f in stale.iter().chain(&remade) {
                doc[f] = Value::Null;
            }
            // A semantic ID whose vector changes is derived again (the
            // writer keeps it if the levels stay the same).
            for (f, vf) in crate::sid::fields(schema) {
                if stale.contains(&vf) || sets.iter().any(|(g, _)| *g == vf) {
                    doc[f] = Value::Null;
                }
            }
            schema.fill_generated(&mut doc);
            rewrites.push(doc);
        }
        n += 1;
    }
    Ok(Planned { in_place, rewrites, rows: n })
}

/// The fields a rewrite setting `sets` leaves for a model to make again:
/// the derived fields whose files it changes, unless it sets their text
/// itself, then the embedded fields whose sources (or the text made again)
/// it changes, unless it sets their vectors itself.
fn cleared(schema: &Schema, models: &[Option<Arc<dyn Embedder>>], derivers: &[Option<Arc<dyn Deriver>>], sets: &[(usize, Assign)]) -> (Vec<usize>, Vec<usize>) {
    let remade: Vec<usize> = (0..schema.fields.len())
        .filter(|&f| derivers[f].is_some() && !sets.iter().any(|(g, _)| *g == f))
        .filter(|&f| sets.iter().any(|(g, _)| schema.derives_from(f) == Some(*g)))
        .collect();
    let stale: Vec<usize> = (0..schema.fields.len())
        .filter(|&f| models[f].is_some() && !sets.iter().any(|(g, _)| *g == f))
        .filter(|&f| {
            let from = &schema.fields[f].embed.as_ref().expect("embedded").from;
            sets.iter().map(|(g, _)| *g).chain(remade.iter().copied()).any(|g| from.iter().any(|s| schema.field_index(s) == Some(g)) || generated_from_set(schema, from, g))
        })
        .collect();
    (remade, stale)
}

/// Whether rows rewritten by setting `sets` may need a model: one makes
/// a field again ([`cleared`]), or a field that one makes is set to
/// something other than a value (NULL, an expression, the proposed
/// row's), which may leave it for the model. Otherwise the vectors and the
/// text the rows hold stay theirs, and nothing is computed ahead of the
/// writer lock. A row that lacks one already still gets it under the lock.
fn sets_need_models(schema: &Schema, models: &[Option<Arc<dyn Embedder>>], derivers: &[Option<Arc<dyn Deriver>>], sets: &[(usize, Assign)]) -> bool {
    let (remade, stale) = cleared(schema, models, derivers, sets);
    !remade.is_empty() || !stale.is_empty() || sets.iter().any(|(f, a)| (models[*f].is_some() || derivers[*f].is_some()) && !matches!(a, Assign::Value(v) if !matches!(v, Value::Null)))
}

/// The keys of the rows `select` reads, and the fields it reads of them
/// (positions), when it reads rows by primary key only: each projected
/// column a field, the filter `key = v`, `key IN (...)` or a disjunction
/// of them, with values of the key's type, and nothing else but a LIMIT
/// and OFFSET (no order, groups, graphs, guard or rerank phases), on
/// options that search every segment. Each key once, in the order the
/// filter names them. `None` for any other query, which the executor
/// runs.
fn keys_read(schema: &Schema, select: &Select, opts: &ExecOptions) -> Option<(Vec<Value>, Vec<usize>)> {
    use crate::query::logical::{Pred, Projection};
    fn pinned(p: &Pred, pk: usize, out: &mut Vec<Value>) -> bool {
        match p {
            Pred::In { field, values } if *field == pk => {
                out.extend(values.iter().cloned());
                true
            }
            Pred::IntRange { field, lo, hi } if *field == pk && lo == hi => {
                out.push(Value::Int(*lo));
                true
            }
            Pred::False => true,
            Pred::Or(ps) => ps.iter().all(|p| pinned(p, pk, out)),
            _ => false,
        }
    }
    let pk = schema.primary_key?;
    let plain = select.order.is_none()
        && select.tiebreak.is_none()
        && select.group_by.is_empty()
        && select.having.is_none()
        && select.paths.is_empty()
        && select.graphs.is_empty()
        && select.phases.is_empty()
        && select.guard.is_none()
        && select.per.is_none()
        && opts.segments.is_none();
    if !plain {
        return None;
    }
    let fields = select
        .projection
        .iter()
        .map(|(_, p)| match p {
            Projection::Field(f) => Some(*f),
            _ => None,
        })
        .collect::<Option<Vec<usize>>>()?;
    let mut keys = Vec::new();
    if !pinned(&select.filter, pk, &mut keys) {
        return None;
    }
    let typed = match schema.fields[pk].ty {
        FieldType::Keyword => keys.iter().all(|k| matches!(k, Value::Str(_))),
        FieldType::Int => keys.iter().all(|k| matches!(k, Value::Int(_))),
        _ => false,
    };
    if !typed {
        return None;
    }
    let mut seen = HashSet::new();
    keys.retain(|k| seen.insert(Part::of(k)));
    Some((keys, fields))
}

/// The rows an UPDATE filling in embedded field `backfill`'s vectors
/// writes ([`Db::prepare_backfill`]): those still without one whose
/// sources are not all empty. Every row of any other UPDATE.
fn lacking(schema: &Schema, backfill: Option<usize>, mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    if let Some(f) = backfill {
        rows.retain(|r| {
            let mut d = r.clone();
            schema.fill_generated(&mut d);
            matches!(r[f], Value::Null) && !schema.embed_text(f, &d).is_empty()
        });
    }
    rows
}

/// Whether setting `sets` changes rows in place: every field set is a
/// single-valued number.
fn in_place(schema: &Schema, sets: &[(usize, Assign)]) -> bool {
    sets.iter().all(|(f, _)| matches!(schema.fields[*f].ty, FieldType::Int | FieldType::Float) && !schema.fields[*f].multi)
}

/// Sets fields of live rows ([`plan_sets`]), making the text and the
/// vectors the rewrites leave out (from `known` where it has them).
fn set_rows(
    tt: &mut TableTxn,
    schema: &Schema,
    (models, derivers): (&[Option<Arc<dyn Embedder>>], &[Option<Arc<dyn Deriver>>]),
    rows: Vec<(Vec<Value>, Document)>,
    sets: &[(usize, Assign)],
    (known, derived): (&Known, &Derived),
    files: &Files,
) -> Result<u64> {
    let mut p = plan_sets(schema, models, derivers, rows, sets)?;
    for (key, f, v) in p.in_place {
        tt.set(&key, &schema.fields[f].name, v)?;
    }
    derive_known(derivers, schema, &mut p.rewrites, derived, files)?;
    // The rest of each row was read back: its multi-vector values were
    // pooled when they were written. A multi-vector field embedded again
    // (its text changed) is new, as the fields set are.
    let embedded: Vec<usize> = (0..schema.fields.len()).filter(|&f| schema.fields[f].multi && models.get(f).is_some_and(Option::is_some)).collect();
    let fresh: Vec<Vec<usize>> = p.rewrites.iter().map(|d| embedded.iter().copied().filter(|&f| matches!(d[f], Value::Null)).collect()).collect();
    embed_known(models, schema, &mut p.rewrites, known, files)?;
    let set: Vec<usize> = sets.iter().map(|(f, _)| *f).collect();
    for (doc, fresh) in p.rewrites.into_iter().zip(fresh) {
        let assigned: Vec<usize> = set.iter().copied().chain(fresh).collect();
        tt.rewrite(doc, &assigned)?;
    }
    Ok(p.rows)
}

/// The documents of an INSERT whose key is neither live nor proposed
/// earlier by the same statement (`INSERT OR IGNORE`, `ON CONFLICT DO
/// NOTHING`).
fn not_live(snap: &Snapshot, pk: usize, mut docs: Vec<Document>) -> Vec<Document> {
    let mut seen = HashSet::new();
    docs.retain(|d| snap.find(&d[pk]).is_none() && seen.insert(Part::of(&d[pk])));
    docs
}

/// `e`'s vectors of the tokens of `texts` ([`Embedder::embed_tokens`]),
/// each text's one after another, split across the cores as
/// [`embed_all`] splits them.
fn embed_all_tokens(e: &dyn Embedder, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
    let threads = if e.parallel() { std::thread::available_parallelism().map_or(1, |n| n.get()).min(refs.len() / 16).max(1) } else { 1 };
    let flat = |c: &[&str]| -> Result<Vec<Vec<f32>>> { Ok(e.embed_tokens(c)?.into_iter().map(|vs| vs.concat()).collect()) };
    let vectors: Vec<Vec<f32>> = if threads == 1 {
        flat(&refs)?
    } else {
        std::thread::scope(|s| {
            let parts: Vec<_> = refs.chunks(refs.len().div_ceil(threads)).map(|c| s.spawn(move || flat(c))).collect();
            parts.into_iter().map(|p| p.join().expect("embedding thread")).collect::<Result<Vec<_>>>()
        })?
        .into_iter()
        .flatten()
        .collect()
    };
    if vectors.len() != texts.len() {
        return Err(Error::Schema(format!("model {} made the tokens of {} texts of {}", e.name(), vectors.len(), texts.len())));
    }
    Ok(vectors)
}

/// `e`'s vectors of `texts`, split across the cores for a model computed
/// here ([`Embedder::parallel`]).
fn embed_all(e: &dyn Embedder, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
    let threads = if e.parallel() { std::thread::available_parallelism().map_or(1, |n| n.get()).min(refs.len() / 64).max(1) } else { 1 };
    let vectors = if threads == 1 {
        e.embed(&refs)?
    } else {
        std::thread::scope(|s| {
            let parts: Vec<_> = refs.chunks(refs.len().div_ceil(threads)).map(|c| s.spawn(move || e.embed(c))).collect();
            parts.into_iter().map(|p| p.join().expect("embedding thread")).collect::<Result<Vec<_>>>()
        })?
        .into_iter()
        .flatten()
        .collect()
    };
    if vectors.len() != texts.len() {
        return Err(Error::Schema(format!("model {} made {} vectors of {} texts", e.name(), vectors.len(), texts.len())));
    }
    Ok(vectors)
}

fn with_source(mut opts: DbOptions) -> DbOptions {
    if opts.doc_source.is_none() {
        opts.doc_source = Some(Arc::new(StoredSource));
    }
    opts
}

/// Rebuilds documents for merges: stored fields from the doc store, vectors
/// and unstored structured fields from their indexes, generated text from
/// its sources. Unstored TEXT or SPARSE has none, so a merge of a schema
/// with one (from before [`Schema::new`] refused them) fails instead of
/// emptying its index.
pub struct StoredSource;

impl DocumentSource for StoredSource {
    fn documents(&self, schema: &Schema, seg: &SegmentFile, docs: &[DocId]) -> Result<Vec<Document>> {
        if let Some(f) = schema.unrebuildable() {
            return Err(crate::Error::Schema(format!("cannot merge: {} is neither stored nor generated, so a merge would erase its index", f.name)));
        }
        let store = if schema.fields.iter().any(|f| f.stored) { Some(DocStore::open(seg, schema)?) } else { None };
        let mut vectors = Vec::new();
        let mut numeric = Vec::new();
        let mut keyword = Vec::new();
        let mut edges = Vec::new();
        for (i, f) in schema.fields.iter().enumerate() {
            match f.ty {
                FieldType::Edges if !f.stored && f.indexed => {
                    if let Some(g) = crate::graph::GraphReader::open(seg, &f.key())? {
                        edges.push((i, g));
                    }
                }
                FieldType::Vector { .. } => vectors.push((i, VectorReader::open(seg, &f.key())?)),
                FieldType::Int | FieldType::Float if !f.stored && f.indexed => numeric.push((i, NumericColumn::open(seg, &f.key())?)),
                FieldType::Keyword if !f.stored && f.indexed => keyword.push((i, KeywordColumn::open(seg, &f.key())?)),
                _ => {}
            }
        }
        let mut out = Vec::with_capacity(docs.len());
        // Documents in docID order: each block decompressed once.
        let mut cache = crate::forward::StoreCache::default();
        for &d in docs {
            let mut doc = match &store {
                Some(s) => s.get_cached(d, &mut cache)?,
                None => vec![Value::Null; schema.fields.len()],
            };
            for (i, v) in &vectors {
                match v.row_value(d) {
                    Value::Null => {}
                    x => doc[*i] = x,
                }
            }
            for (i, c) in &numeric {
                doc[*i] = c.get(d);
            }
            for (i, k) in &keyword {
                doc[*i] = k.value_of_doc(d).map_or(Value::Null, |v| Value::Str(v.to_string()));
            }
            for (i, g) in &edges {
                doc[*i] = g.edges_value(d);
            }
            schema.fill_generated(&mut doc);
            out.push(doc);
        }
        Ok(out)
    }
}

/// The plan of a SELECT over `snap`'s tables: the executor's query of one
/// table, or a compound one's operators ([`compound::needs`]) over any
/// ([`compound::plan_read`]).
/// Texts a query gives where vectors go, by this database's models.
impl crate::query::embed_sql::Embeds for Db {
    fn field_query(&self, table: &str, field: &str, text: &str) -> Result<Value> {
        self.embed_query_in(table, field, text)
    }
    fn model_query(&self, model: &str, text: &str) -> Result<Vec<f32>> {
        self.named_model(model)?.embed_query(text)
    }
}

fn plan_of(snap: &Snapshot, q: &sqlparser::ast::Query, params: &Params, functions: &FunctionSet) -> Result<Plan> {
    let catalog: Vec<&Schema> = snap.schemas().collect();
    compound::plan_read_query(q, &catalog, params, &|t, name| snap.table(t).and_then(|v| v.rank_profile(name).cloned()), functions)
}

/// [`plan_of`] of a query [`qsql::parse_query_cached`] parsed.
fn plan_of_parsed(snap: &Snapshot, q: &qsql::Parsed, params: &Params, functions: &FunctionSet) -> Result<Plan> {
    let catalog: Vec<&Schema> = snap.schemas().collect();
    compound::plan_read_parsed(q, &catalog, params, &|t, name| snap.table(t).and_then(|v| v.rank_profile(name).cloned()), functions)
}


/// Opening segments' readers takes microseconds once they are cached; a
/// step when it took longer (a cold cache, new segments).
fn opened_step(stats: &mut crate::query::exec::ExecStats, t0: std::time::Instant, opened: std::time::Duration, segments: usize) {
    if opened.as_micros() >= 20 {
        let step = crate::query::steps::Step { op: "open readers".into(), detail: format!("{segments} segments"), start_us: crate::query::steps::us_at(t0), us: crate::query::steps::tenth(opened.as_nanos() as f64 / 1e3), ..Default::default() };
        stats.steps.insert(0, step);
    }
}

/// The table of `catalog` (a database's tables, the first first) that a
/// write or a read naming no table goes to ([`Db::default_table`]): the
/// first table that is not the placeholder of a database made with no
/// tables, if any.
pub fn default_table(catalog: &[Arc<Schema>]) -> Option<Arc<Schema>> {
    catalog.iter().find(|t| !t.is_placeholder()).cloned()
}

/// A text corrected by [`Db::correct`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Corrected {
    /// The text with its typos replaced.
    pub text: String,
    /// Whether any word was replaced.
    pub changed: bool,
    pub words: Vec<crate::query::exec::Correction>,
}
