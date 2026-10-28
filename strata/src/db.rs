//! The database: storage plus the query engine, behind SQL.
//!
//! ```no_run
//! # use strata::{db::Db, query::Params, storage::DbOptions, data};
//! let db = Db::open(std::path::Path::new("papers.strata"), DbOptions::default()).unwrap();
//! let rows = db.query("SELECT id, score() FROM docs ORDER BY bm25(body, 'mrna vaccine') DESC LIMIT 10", &Params::new()).unwrap();
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::forward::{DocStore, KeywordColumn, NumericColumn};
use crate::query::exec::{ExecOptions, Executor, ResultSet, SegmentReaders};
use crate::query::stats::TermStats;
use crate::query::{bind, bind_write, Assign, Params, Select, Write};
use crate::schema::{FieldType, Schema};
use crate::segment::{DocId, SegmentFile};
use crate::storage::{Database, DbOptions, DocumentSource, Snapshot};
use crate::value::{Document, Value};
use crate::vector::VectorReader;

/// What [`Db::sql`] returns.
#[derive(Debug)]
pub enum SqlResult {
    Rows(ResultSet),
    /// Documents inserted, updated or deleted.
    Written(u64),
}

pub struct Db {
    storage: Arc<Database>,
    /// Readers by segment id, with the address of the image they read.
    /// On-disk segments keep their id for life (deletes travel with the
    /// snapshot, not the readers), but a vacuum can move their bytes;
    /// in-memory ones get a fresh id per rebuild.
    readers: RwLock<HashMap<u64, (usize, Arc<SegmentReaders>)>>,
    pub exec: ExecOptions,
    /// Models of embedded VECTOR fields, by field index ([`Db::set_embedder`]).
    embedders: RwLock<Vec<Option<Arc<dyn Embedder>>>>,
}

impl Db {
    pub fn create(path: &Path, schema: Schema, opts: DbOptions) -> Result<Db> {
        Ok(Db::wrap(Database::create(path, schema, with_source(opts))?))
    }

    pub fn open(path: &Path, opts: DbOptions) -> Result<Db> {
        Ok(Db::wrap(Database::open(path, with_source(opts))?))
    }

    fn wrap(storage: Database) -> Db {
        let n = storage.schema().fields.len();
        Db { storage: Arc::new(storage), readers: RwLock::new(HashMap::new()), exec: ExecOptions::default(), embedders: RwLock::new(vec![None; n]) }
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

    /// The schema of the database's first table, which SQL and the other
    /// methods here query and write.
    pub fn schema(&self) -> &Schema {
        self.storage.schema()
    }

    /// The schemas of the database's tables, the first one first
    /// ([`Database::tables`]).
    pub fn catalog(&self) -> Vec<Arc<Schema>> {
        self.storage.tables()
    }

    /// Adds an empty table ([`Database::create_table`]). Its rows are
    /// written with [`crate::storage::WriteTxn::table`] and read with
    /// [`Snapshot::table`].
    pub fn create_table(&self, schema: Schema) -> Result<()> {
        self.storage.create_table(schema)
    }

    /// Drops a table other than the first ([`Database::drop_table`]).
    pub fn drop_table(&self, name: &str) -> Result<()> {
        self.storage.drop_table(name)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.storage.snapshot()
    }

    /// Bulk-loads documents (generated columns are filled in, and embedded
    /// vectors the documents leave out are computed).
    pub fn load(&self, docs: impl IntoIterator<Item = Document>) -> Result<()> {
        self.try_load(docs.into_iter().map(Ok))
    }

    /// [`Db::load`] of documents that may fail to arrive (rows read from a
    /// file): the first error ends the load, and nothing is committed.
    pub fn try_load(&self, docs: impl IntoIterator<Item = Result<Document>>) -> Result<()> {
        let schema = self.schema().clone();
        let mut docs = docs.into_iter();
        if !self.embeds() {
            return self.storage.try_bulk_load(docs.map(move |d| {
                d.map(|mut d| {
                    schema.fill_generated(&mut d);
                    d
                })
            }));
        }
        // Embedded in batches, so that a model sees many texts at once.
        let mut ready: std::vec::IntoIter<Result<Document>> = Vec::new().into_iter();
        self.storage.try_bulk_load(std::iter::from_fn(|| loop {
            if let Some(d) = ready.next() {
                return Some(d);
            }
            let mut batch = Vec::new();
            for d in docs.by_ref().take(EMBED_BATCH) {
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
            self.embed_missing(&mut batch);
            ready = batch.into_iter().map(Ok).collect::<Vec<_>>().into_iter();
        }))
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

    /// Bulk-loads the rows of `other`, a database with another schema,
    /// matching fields by name: to add, drop or re-embed fields, create a
    /// database with the new schema and load the old one into it. A VECTOR
    /// field is copied when the old field has the same dimensions and
    /// embedding ([`crate::schema::Embed`]); otherwise this database embeds
    /// it anew (given the model, [`Db::set_embedder`]). Returns the rows
    /// loaded.
    pub fn load_from(&self, other: &Db) -> Result<u64> {
        let (old, new) = (other.schema(), self.schema());
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
                    (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b) && f.multi == g.multi,
                };
                keep.then_some(j)
            })
            .collect();
        let snap = other.snapshot();
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
        self.load(docs)?;
        match failed {
            Some(e) => Err(e),
            None => Ok(n),
        }
    }

    /// Gives embedded VECTOR field `field` ([`crate::schema::Embed`]) its
    /// model. Writes through this `Db` that leave the vector out get it
    /// computed ([`Db::load`], SQL `INSERT` and `UPDATE`,
    /// [`Db::embed_missing`] before a [`crate::storage::WriteTxn`]).
    pub fn set_embedder(&self, field: &str, e: Arc<dyn Embedder>) -> Result<()> {
        let schema = self.schema();
        let i = schema.field_index(field).ok_or_else(|| Error::Schema(format!("no field {field}")))?;
        let f = &schema.fields[i];
        let (FieldType::Vector { dim, .. }, Some(spec)) = (&f.ty, &f.embed) else {
            return Err(Error::Schema(format!("field {field} is not an embedded VECTOR field")));
        };
        if e.dim() != *dim {
            return Err(Error::Schema(format!("model {} makes {}-dimension vectors; field {field} holds {dim}", spec.model, e.dim())));
        }
        self.embedders.write().unwrap_or_else(|e| e.into_inner())[i] = Some(e);
        Ok(())
    }

    /// The model of embedded field `field`, if it has one.
    pub fn embedder(&self, field: &str) -> Option<Arc<dyn Embedder>> {
        let i = self.schema().field_index(field)?;
        self.embedders.read().unwrap_or_else(|e| e.into_inner())[i].clone()
    }

    /// Opens the model of every embedded field from `models`
    /// ([`crate::embed::open`]); returns the fields whose model did not
    /// open, with the reason. Their vectors must come with the writes.
    pub fn open_embedders(&self, models: &std::path::Path) -> Vec<(String, Error)> {
        let mut failed = Vec::new();
        for f in &self.schema().fields {
            let Some(spec) = &f.embed else { continue };
            if let Err(e) = crate::embed::open(&spec.model, models).and_then(|m| self.set_embedder(&f.name, m)) {
                failed.push((f.name.clone(), e));
            }
        }
        failed
    }

    fn embeds(&self) -> bool {
        self.embedders.read().unwrap_or_else(|e| e.into_inner()).iter().any(|e| e.is_some())
    }

    /// Computes the embedded vectors `docs` leave out (null) from their
    /// sources, one batch per field split across the cores. A document
    /// whose sources are all empty gets no vector. Generated fields must be
    /// filled in first.
    pub fn embed_missing(&self, docs: &mut [Document]) {
        let embedders = self.embedders.read().unwrap_or_else(|e| e.into_inner()).clone();
        let schema = self.schema();
        for (f, e) in embedders.iter().enumerate() {
            let Some(e) = e else { continue };
            let mut todo = Vec::new();
            let mut texts = Vec::new();
            for (j, d) in docs.iter().enumerate() {
                if matches!(d[f], Value::Null) {
                    let t = schema.embed_text(f, d);
                    if !t.is_empty() {
                        todo.push(j);
                        texts.push(t);
                    }
                }
            }
            for (j, v) in todo.into_iter().zip(embed_all(e.as_ref(), &texts)) {
                docs[j][f] = Value::Vector(v.into());
            }
        }
    }

    pub fn query(&self, sql: &str, params: &Params) -> Result<ResultSet> {
        self.query_with(sql, params, &self.exec)
    }

    pub fn query_with(&self, sql: &str, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
        self.query_at(&self.snapshot(), sql, params, opts)
    }

    /// Runs a query against a given snapshot (several queries that must
    /// see the same version share one).
    ///
    /// A compound query (`WITH`, a subquery, a join, `SELECT DISTINCT`) runs
    /// as several single-table queries on the same snapshot. Its steps may
    /// hold up to [`crate::query::compound::MAX_ROWS`] rows each; its answer
    /// is held to [`ExecOptions::max_rows`].
    pub fn query_at(&self, snap: &Snapshot, sql: &str, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
        if crate::query::compound::is_compound(sql, snap.schema()) {
            let steps = ExecOptions { max_rows: None, ..opts.clone() };
            let rs = crate::query::compound::run_checked(
                sql,
                snap.schema(),
                params,
                &mut |text, p| {
                    let select = bind(text, snap.schema(), p)?;
                    self.execute(snap, &select, &steps)
                },
                opts,
            )?;
            if let Some(max) = opts.max_rows.filter(|&m| rs.rows.len() > m) {
                return Err(Error::Query(format!("the query matches more than {max} rows, the most a query returns; add a LIMIT")));
            }
            return Ok(rs);
        }
        let select = bind(sql, snap.schema(), params)?;
        self.execute(snap, &select, opts)
    }

    /// Runs any statement: a SELECT returns its rows, an INSERT, UPDATE or
    /// DELETE the number of documents it wrote, in one transaction.
    ///
    /// UPDATE sets numeric fields in place ([`crate::storage::WriteTxn::set`])
    /// and rewrites the whole document for any other field, which needs every
    /// text field stored or generated. The rows to change are read after the
    /// transaction takes the writer lock, so no other write lands between.
    pub fn sql(&self, sql: &str, params: &Params) -> Result<SqlResult> {
        let Some(write) = bind_write(sql, self.schema(), params)? else {
            return self.query(sql, params).map(SqlResult::Rows);
        };
        let schema = self.schema().clone();
        let mut t = self.storage.begin()?;
        let mut n = 0u64;
        match write {
            Write::Insert { mut docs, replace } => {
                docs.iter_mut().for_each(|d| schema.fill_generated(d));
                self.embed_missing(&mut docs);
                for d in docs {
                    if replace {
                        t.upsert(d)?;
                    } else {
                        t.insert(d)?;
                    }
                    n += 1;
                }
            }
            Write::Delete { select } => {
                let pk = schema.primary_key.ok_or_else(|| crate::Error::Query("DELETE needs a primary key".into()))?;
                for row in self.execute(&self.snapshot(), &select, &self.exec)?.rows {
                    n += t.delete(&row[pk])? as u64;
                }
            }
            Write::Update { select, sets } => {
                let pk = schema.primary_key.ok_or_else(|| crate::Error::Query("UPDATE needs a primary key".into()))?;
                let in_place = |f: usize| {
                    let fld = &schema.fields[f];
                    matches!(fld.ty, FieldType::Int | FieldType::Float) && !fld.multi
                };
                if !sets.iter().all(|(f, _)| in_place(*f)) {
                    if let Some(f) = schema.fields.iter().find(|f| matches!(f.ty, FieldType::Text { .. }) && !f.stored && f.generated_from.is_empty()) {
                        return Err(crate::Error::Query(format!("UPDATE of non-numeric fields rewrites documents, and {} is not stored", f.name)));
                    }
                }
                // A rewrite that changes a source of an embedded field
                // recomputes its vector, unless it sets the vector itself.
                let stale: Vec<usize> = {
                    let embedders = self.embedders.read().unwrap_or_else(|e| e.into_inner());
                    (0..schema.fields.len())
                        .filter(|&f| embedders[f].is_some() && !sets.iter().any(|(g, _)| *g == f))
                        .filter(|&f| {
                            let from = &schema.fields[f].embed.as_ref().expect("embedded").from;
                            sets.iter().any(|(g, _)| from.iter().any(|s| schema.field_index(s) == Some(*g)) || generated_from_set(&schema, from, *g))
                        })
                        .collect()
                };
                let mut rewrites = Vec::new();
                for row in self.execute(&self.snapshot(), &select, &self.exec)?.rows {
                    let new = |f: usize, a: &Assign| -> Result<Value> {
                        Ok(match a {
                            Assign::Value(v) => v.clone(),
                            // NULL counts as zero; a sum that overflows is an
                            // error, as in Postgres.
                            Assign::Add(x) => {
                                let overflow = || crate::Error::Query(format!("{} + {x:?} overflows", schema.fields[f].name));
                                match (&schema.fields[f].ty, x) {
                                    (FieldType::Int, Value::Int(x)) => Value::Int(row[f].as_i64().unwrap_or(0).checked_add(*x).ok_or_else(overflow)?),
                                    (FieldType::Int, Value::Float(y)) if y.fract() == 0.0 && y.abs() < 9.2e18 => {
                                        Value::Int(row[f].as_i64().unwrap_or(0).checked_add(*y as i64).ok_or_else(overflow)?)
                                    }
                                    (FieldType::Int, _) => return Err(crate::Error::Query(format!("{} is INT: add an integer", schema.fields[f].name))),
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
                    if sets.iter().all(|(f, _)| in_place(*f)) {
                        for (f, a) in &sets {
                            t.set(&row[pk], &schema.fields[*f].name, new(*f, a)?)?;
                        }
                    } else {
                        let mut doc = row.clone();
                        for (f, a) in &sets {
                            doc[*f] = new(*f, a)?;
                        }
                        for &f in &stale {
                            doc[f] = Value::Null;
                        }
                        schema.fill_generated(&mut doc);
                        rewrites.push(doc);
                    }
                    n += 1;
                }
                self.embed_missing(&mut rewrites);
                for doc in rewrites {
                    t.upsert(doc)?;
                }
            }
        }
        t.commit()?;
        Ok(SqlResult::Written(n))
    }

    pub fn execute(&self, snap: &Snapshot, select: &Select, opts: &ExecOptions) -> Result<ResultSet> {
        let readers = self.readers(snap)?;
        Executor::new(snap, &readers, opts).execute(select)
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
        Executor::new(snap, &readers, opts).term_stats(select)
    }

    /// Builds a segment image of `docs` for this schema into `dest` (a new
    /// file), to attach to a database elsewhere ([`Db::attach_segments`]).
    pub fn write_segment(schema: &Schema, docs: Vec<Document>, opts: &crate::segment::build::BuildOptions, dest: &Path) -> Result<()> {
        let docs: Vec<Document> = docs
            .into_iter()
            .map(|mut d| {
                schema.fill_generated(&mut d);
                d
            })
            .collect();
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
            let r = SegmentReaders::open(&file, &schema)?;
            for (i, f) in schema.fields.iter().enumerate() {
                let ok = match f.ty {
                    FieldType::Text { .. } | FieldType::Sparse => r.text(i).map(|_| ()),
                    FieldType::Keyword => r.keyword(i).map(|_| ()),
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
        {
            let cache = self.readers.read().unwrap_or_else(|e| e.into_inner());
            let hit = |v: &crate::storage::SegmentView| cache.get(&v.id).filter(|(p, _)| *p == at(v)).map(|(_, r)| r.clone());
            if let Some(all) = snap.segments().iter().map(hit).collect() {
                return Ok(all);
            }
        }
        let mut cache = self.readers.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::with_capacity(snap.segments().len());
        for v in snap.segments() {
            let r = match cache.get(&v.id).filter(|(p, _)| *p == at(v)) {
                Some((_, r)) => r.clone(),
                None => {
                    let r = Arc::new(SegmentReaders::open(&v.file, snap.schema())?);
                    cache.insert(v.id, (at(v), r.clone()));
                    r
                }
            };
            out.push(r);
        }
        if cache.len() > 2 * snap.segments().len() + 16 {
            let live: std::collections::HashSet<u64> = snap.segments().iter().map(|v| v.id).collect();
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

/// Whether field `g` is a source of a generated field among `from`.
fn generated_from_set(schema: &Schema, from: &[String], g: usize) -> bool {
    from.iter().filter_map(|s| schema.field(s)).any(|f| f.generated_from.iter().any(|x| schema.field_index(x) == Some(g)))
}

/// `e`'s vectors of `texts`, split across the cores.
fn embed_all(e: &dyn Embedder, texts: &[String]) -> Vec<Vec<f32>> {
    let refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(refs.len() / 64).max(1);
    if threads == 1 {
        return e.embed(&refs);
    }
    std::thread::scope(|s| {
        let parts: Vec<_> = refs.chunks(refs.len().div_ceil(threads)).map(|c| s.spawn(move || e.embed(c))).collect();
        parts.into_iter().flat_map(|p| p.join().expect("embedding thread")).collect()
    })
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
                    if let Some(g) = crate::graph::GraphReader::open(seg, &f.name)? {
                        edges.push((i, g));
                    }
                }
                FieldType::Vector { .. } => vectors.push((i, VectorReader::open(seg, &f.name)?)),
                FieldType::Int | FieldType::Float if !f.stored && f.indexed => numeric.push((i, NumericColumn::open(seg, &f.name)?)),
                FieldType::Keyword if !f.stored && f.indexed => keyword.push((i, KeywordColumn::open(seg, &f.name)?)),
                _ => {}
            }
        }
        let mut out = Vec::with_capacity(docs.len());
        for &d in docs {
            let mut doc = match &store {
                Some(s) => s.get(d),
                None => vec![Value::Null; schema.fields.len()],
            };
            for (i, v) in &vectors {
                if v.has_vector(d) {
                    doc[*i] = Value::Vector(v.vector(d).into());
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
