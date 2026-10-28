//! The database: storage plus the query engine, behind SQL.
//!
//! ```no_run
//! # use strata::{db::Db, query::Params, storage::DbOptions, data};
//! let db = Db::open(std::path::Path::new("papers.strata"), DbOptions::default()).unwrap();
//! let rows = db.query("SELECT id, score() FROM docs ORDER BY bm25(body, 'mrna vaccine') DESC LIMIT 10", &Params::new()).unwrap();
//! ```
//!
//! SQL reads and writes every table of the database by name, and creates
//! and drops tables (`CREATE TABLE`, `DROP TABLE`); a query reads one table.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::forward::{DocStore, KeywordColumn, NumericColumn};
use crate::query::agg::Part;
use crate::query::exec::{ExecOptions, Executor, ResultSet, SegmentReaders};
use crate::query::stats::TermStats;
use crate::query::{bind, bind_command, compound, sql as qsql, tables, Assign, Command, Conflict, Params, Select, Write};
use crate::schema::{Embed, FieldType, Schema};
use crate::segment::{DocId, SegmentFile};
use crate::storage::{Database, DbOptions, DocumentSource, DropOptions, Snapshot, TableTxn};
use crate::value::{Document, Value};
use crate::vector::VectorReader;

/// What [`Db::sql`] returns.
#[derive(Debug)]
pub enum SqlResult {
    Rows(ResultSet),
    /// Documents inserted, updated or deleted (0 for `CREATE TABLE` and
    /// `DROP TABLE`).
    Written(u64),
}

pub struct Db {
    storage: Database,
    /// Readers by segment id, with the address of the image they read.
    /// On-disk segments keep their id for life (deletes travel with the
    /// snapshot, not the readers), but a vacuum can move their bytes;
    /// in-memory ones get a fresh id per rebuild.
    readers: RwLock<HashMap<u64, (usize, Arc<SegmentReaders>)>>,
    pub exec: ExecOptions,
    /// Models of embedded VECTOR fields ([`Db::set_embedder_in`]).
    embedders: RwLock<Vec<Model>>,
    /// Where [`Db::open_embedders`] found models, for the tables created
    /// after it.
    models: RwLock<Option<PathBuf>>,
}

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

impl Db {
    pub fn create(path: &Path, schema: Schema, opts: DbOptions) -> Result<Db> {
        Ok(Db::wrap(Database::create(path, schema, with_source(opts))?))
    }

    pub fn open(path: &Path, opts: DbOptions) -> Result<Db> {
        Ok(Db::wrap(Database::open(path, with_source(opts))?))
    }

    fn wrap(storage: Database) -> Db {
        Db { storage, readers: RwLock::new(HashMap::new()), exec: ExecOptions::default(), embedders: RwLock::new(Vec::new()), models: RwLock::new(None) }
    }

    pub fn storage(&self) -> &Database {
        &self.storage
    }

    /// The schema of the database's first table, which the methods here
    /// that name no table write.
    pub fn schema(&self) -> &Schema {
        self.storage.schema()
    }

    /// The schemas of the database's tables, the first one first
    /// ([`Database::tables`]).
    pub fn catalog(&self) -> Vec<Arc<Schema>> {
        self.storage.tables()
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
    pub fn create_table(&self, schema: Schema) -> Result<()> {
        let name = schema.table.clone();
        self.storage.create_table(schema)?;
        let dir = self.models.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(dir) = dir {
            self.open_embedders_of(&name, &dir);
        }
        Ok(())
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
        let models = self.models_of(&schema);
        let mut docs = docs.into_iter();
        if models.iter().all(Option::is_none) {
            return self.storage.try_bulk_load_into(table, docs.map(move |d| {
                d.map(|mut d| {
                    schema.fill_generated(&mut d);
                    d
                })
            }));
        }
        // Embedded in batches, so that a model sees many texts at once.
        let mut ready: std::vec::IntoIter<Result<Document>> = Vec::new().into_iter();
        self.storage.try_bulk_load_into(
            table,
            std::iter::from_fn(|| loop {
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
                embed_with(&models, &schema, &mut batch);
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

    /// Gives embedded VECTOR field `field` ([`crate::schema::Embed`]) of the
    /// first table its model. Writes through this `Db` that leave the vector
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
        let (FieldType::Vector { dim, .. }, Some(spec)) = (&f.ty, &f.embed) else {
            return Err(Error::Schema(format!("field {field} is not an embedded VECTOR field")));
        };
        if e.dim() != *dim {
            return Err(Error::Schema(format!("model {} makes {}-dimension vectors; field {field} holds {dim}", spec.model, e.dim())));
        }
        let mut all = self.embedders.write().unwrap_or_else(|e| e.into_inner());
        all.retain(|m| !(m.table.eq_ignore_ascii_case(&schema.table) && m.field.eq_ignore_ascii_case(&f.name)));
        all.push(Model { table: schema.table.clone(), field: f.name.clone(), embed: spec.clone(), dim: *dim, model: e });
        Ok(())
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

    /// The models of the embedded fields of the table `schema` describes,
    /// by field.
    fn models_of(&self, schema: &Schema) -> Vec<Option<Arc<dyn Embedder>>> {
        let all = self.embedders.read().unwrap_or_else(|e| e.into_inner());
        schema
            .fields
            .iter()
            .map(|f| {
                let serves = |m: &&Model| {
                    m.table.eq_ignore_ascii_case(&schema.table) && m.field.eq_ignore_ascii_case(&f.name) && f.embed.as_ref() == Some(&m.embed) && matches!(f.ty, FieldType::Vector { dim, .. } if dim == m.dim)
                };
                all.iter().find(serves).map(|m| m.model.clone())
            })
            .collect()
    }

    /// Opens the model of every embedded field of every table from `models`
    /// ([`crate::embed::open`]), and of the tables created later
    /// ([`Db::create_table`]); returns the fields whose model did not open,
    /// with the reason (named `table.field` outside the first table). Their
    /// vectors must come with the writes.
    pub fn open_embedders(&self, models: &Path) -> Vec<(String, Error)> {
        *self.models.write().unwrap_or_else(|e| e.into_inner()) = Some(models.to_path_buf());
        let first = self.schema().table.clone();
        let mut failed = Vec::new();
        for t in self.catalog() {
            for (f, e) in self.open_embedders_of(&t.table, models) {
                failed.push((if t.table == first { f } else { format!("{}.{f}", t.table) }, e));
            }
        }
        failed
    }

    fn open_embedders_of(&self, table: &str, models: &Path) -> Vec<(String, Error)> {
        let Some(schema) = self.storage.table(table) else { return Vec::new() };
        let mut failed = Vec::new();
        for f in &schema.fields {
            let Some(spec) = &f.embed else { continue };
            if let Err(e) = crate::embed::open(&spec.model, models).and_then(|m| self.set_embedder_in(table, &f.name, m)) {
                failed.push((f.name.clone(), e));
            }
        }
        failed
    }

    /// Computes the embedded vectors that documents of the first table
    /// leave out (null) from their sources, one batch per field split
    /// across the cores. A document whose sources are all empty gets no
    /// vector. Generated fields must be filled in first.
    pub fn embed_missing(&self, docs: &mut [Document]) {
        let schema = self.schema();
        embed_with(&self.models_of(schema), schema, docs);
    }

    /// [`Db::embed_missing`] for documents of the table named `table`.
    pub fn embed_missing_in(&self, table: &str, docs: &mut [Document]) -> Result<()> {
        let schema = self.table(table)?;
        embed_with(&self.models_of(&schema), &schema, docs);
        Ok(())
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
    /// as several single-table queries on the same snapshot. A query reads
    /// one table: joins and subqueries across tables are not supported yet.
    pub fn query_at(&self, snap: &Snapshot, sql: &str, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
        let q = qsql::parse_query(sql)?;
        let view = match tables::read_by(&q).as_slice() {
            // No table: the binder says what is missing.
            [] => snap.clone(),
            [t] => snap.table(t).ok_or_else(|| Error::Query(format!("no such table: {t}")))?,
            [all @ .., last] => {
                let all: Vec<&str> = all.iter().map(|t| t.as_str()).collect();
                return Err(Error::Query(format!(
                    "a query reads one table for now, and this one reads {} and {last}: joins and subqueries across tables are not supported yet",
                    all.join(", ")
                )));
            }
        };
        if compound::needs(&q, view.schema()) {
            return compound::run_query(&q, view.schema(), params, &mut |text, p| {
                let select = bind(text, view.schema(), p)?;
                self.execute(&view, &select, opts)
            });
        }
        let select = qsql::bind_query(&q, view.schema(), params)?;
        self.execute(&view, &select, opts)
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
        let catalog = self.catalog();
        let tables: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
        match bind_command(sql, &tables, params)? {
            Command::Query => self.query(sql, params).map(SqlResult::Rows),
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
            Command::Write { table, write } => {
                let schema = catalog.iter().find(|t| t.table == table).expect("bound to a table of the catalog");
                self.write(schema, write).map(SqlResult::Written)
            }
        }
    }

    /// Runs a bound write of the table `schema` describes, in one
    /// transaction.
    fn write(&self, schema: &Schema, write: Write) -> Result<u64> {
        let mut t = self.storage.begin()?;
        let mut tt = t.table(&schema.table)?;
        if tt.schema() != schema {
            return Err(Error::Query(format!("table {} was dropped and created again while the statement ran; run it again", schema.table)));
        }
        let models = self.models_of(schema);
        // Read under the writer lock: no other write lands between.
        let snap = self.snapshot().table(&schema.table).expect("the writer lock keeps the table");
        let pk = || schema.primary_key.ok_or_else(|| Error::Query(format!("{} has no primary key", schema.table)));
        let mut n = 0u64;
        match write {
            Write::Insert { mut docs, conflict } => {
                docs.iter_mut().for_each(|d| schema.fill_generated(d));
                match conflict {
                    Conflict::Fail | Conflict::Replace => {
                        embed_with(&models, schema, &mut docs);
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
                        let pk = pk()?;
                        // Neither live nor written earlier by this statement.
                        let mut seen = HashSet::new();
                        docs.retain(|d| snap.lookup(&d[pk]).is_none() && seen.insert(Part::of(&d[pk])));
                        embed_with(&models, schema, &mut docs);
                        for d in docs {
                            tt.insert(d)?;
                            n += 1;
                        }
                    }
                    Conflict::Update(sets) => {
                        let pk = pk()?;
                        let mut seen = HashSet::new();
                        if let Some(d) = docs.iter().find(|d| !seen.insert(Part::of(&d[pk]))) {
                            return Err(Error::Query(format!("ON CONFLICT DO UPDATE: key {:?} comes twice; a statement updates a row once", d[pk])));
                        }
                        let (live, mut new): (Vec<Document>, Vec<Document>) = docs.into_iter().partition(|d| snap.lookup(&d[pk]).is_some());
                        embed_with(&models, schema, &mut new);
                        for d in new {
                            tt.insert(d)?;
                            n += 1;
                        }
                        if !live.is_empty() {
                            // The live rows, every field, matched to the documents the INSERT proposed.
                            let cols: Vec<String> = schema.fields.iter().map(|f| qsql::ident(&f.name)).collect();
                            let text = format!("SELECT {} FROM {} WHERE {} IN (:keys)", cols.join(", "), qsql::ident(&schema.table), qsql::ident(&schema.fields[pk].name));
                            let keys = Value::List(live.iter().map(|d| d[pk].clone()).collect());
                            let rows = self.execute(&snap, &bind(&text, schema, &Params::new().set("keys", keys))?, &self.exec)?.rows;
                            let mut by_key: HashMap<Part, Vec<Value>> = rows.into_iter().map(|r| (Part::of(&r[pk]), r)).collect();
                            let pairs: Vec<(Vec<Value>, Document)> = live.into_iter().filter_map(|d| by_key.remove(&Part::of(&d[pk])).map(|r| (r, d))).collect();
                            n += self.set_rows(&mut tt, schema, &models, pairs, &sets)?;
                        }
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
                let rows = self.execute(&snap, &select, &self.exec)?.rows;
                n += self.set_rows(&mut tt, schema, &models, rows.into_iter().map(|r| (r, Vec::new())).collect(), &sets)?;
            }
        }
        drop(tt);
        t.commit()?;
        Ok(n)
    }

    /// Sets fields of live rows: each pair is a row (every field, as a
    /// SELECT of them all returns it) and, for `ON CONFLICT DO UPDATE`, the
    /// document the INSERT proposed ([`Assign::Excluded`]). Only single-valued
    /// numbers change in place; any other field rewrites the row, which
    /// needs every text field stored or generated.
    fn set_rows(&self, tt: &mut TableTxn, schema: &Schema, models: &[Option<Arc<dyn Embedder>>], rows: Vec<(Vec<Value>, Document)>, sets: &[(usize, Assign)]) -> Result<u64> {
        let pk = schema.primary_key.ok_or_else(|| Error::Query(format!("UPDATE needs a primary key, and {} has none", schema.table)))?;
        let in_place = |f: usize| {
            let fld = &schema.fields[f];
            matches!(fld.ty, FieldType::Int | FieldType::Float) && !fld.multi
        };
        let rewrite = !sets.iter().all(|(f, _)| in_place(*f));
        if rewrite {
            if let Some(f) = schema.fields.iter().find(|f| matches!(f.ty, FieldType::Text { .. }) && !f.stored && f.generated_from.is_empty()) {
                return Err(Error::Query(format!("UPDATE of non-numeric fields rewrites documents, and {} is not stored", f.name)));
            }
        }
        // A rewrite that changes a source of an embedded field recomputes
        // its vector, unless it sets the vector itself.
        let stale: Vec<usize> = (0..schema.fields.len())
            .filter(|&f| models[f].is_some() && !sets.iter().any(|(g, _)| *g == f))
            .filter(|&f| {
                let from = &schema.fields[f].embed.as_ref().expect("embedded").from;
                sets.iter().any(|(g, _)| from.iter().any(|s| schema.field_index(s) == Some(*g)) || generated_from_set(schema, from, *g))
            })
            .collect();
        let mut n = 0u64;
        let mut rewrites = Vec::new();
        for (row, proposed) in rows {
            let new = |f: usize, a: &Assign| -> Result<Value> {
                Ok(match a {
                    Assign::Value(v) => v.clone(),
                    Assign::Excluded(g) => proposed[*g].clone(),
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
                    tt.set(&row[pk], &schema.fields[*f].name, new(*f, a)?)?;
                }
            } else {
                let mut doc = row.clone();
                for (f, a) in sets {
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
        embed_with(models, schema, &mut rewrites);
        for doc in rewrites {
            tt.upsert(doc)?;
        }
        Ok(n)
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

/// Whether field `g` is a source of a generated field among `from`.
fn generated_from_set(schema: &Schema, from: &[String], g: usize) -> bool {
    from.iter().filter_map(|s| schema.field(s)).any(|f| f.generated_from.iter().any(|x| schema.field_index(x) == Some(g)))
}

/// Computes the embedded vectors `docs` leave out (null) with `models`
/// (by field of `schema`), one batch per field split across the cores. A
/// document whose sources are all empty gets no vector.
fn embed_with(models: &[Option<Arc<dyn Embedder>>], schema: &Schema, docs: &mut [Document]) {
    for (f, e) in models.iter().enumerate() {
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
