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

use crate::error::Result;
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
    storage: Database,
    /// Readers by segment id, with the address of the image they read.
    /// On-disk segments keep their id for life (deletes travel with the
    /// snapshot, not the readers), but a vacuum can move their bytes;
    /// in-memory ones get a fresh id per rebuild.
    readers: RwLock<HashMap<u64, (usize, Arc<SegmentReaders>)>>,
    pub exec: ExecOptions,
}

impl Db {
    pub fn create(path: &Path, schema: Schema, opts: DbOptions) -> Result<Db> {
        Ok(Db::wrap(Database::create(path, schema, with_source(opts))?))
    }

    pub fn open(path: &Path, opts: DbOptions) -> Result<Db> {
        Ok(Db::wrap(Database::open(path, with_source(opts))?))
    }

    fn wrap(storage: Database) -> Db {
        Db { storage, readers: RwLock::new(HashMap::new()), exec: ExecOptions::default() }
    }

    pub fn storage(&self) -> &Database {
        &self.storage
    }

    pub fn schema(&self) -> &Schema {
        self.storage.schema()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.storage.snapshot()
    }

    /// Bulk-loads documents (generated columns are filled in).
    pub fn load(&self, docs: impl IntoIterator<Item = Document>) -> Result<()> {
        let schema = self.schema().clone();
        self.storage.bulk_load(docs.into_iter().map(move |mut d| {
            schema.fill_generated(&mut d);
            d
        }))
    }

    pub fn query(&self, sql: &str, params: &Params) -> Result<ResultSet> {
        self.query_with(sql, params, &self.exec)
    }

    pub fn query_with(&self, sql: &str, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
        self.query_at(&self.snapshot(), sql, params, opts)
    }

    /// Runs a query against a given snapshot (several queries that must
    /// see the same version share one).
    pub fn query_at(&self, snap: &Snapshot, sql: &str, params: &Params, opts: &ExecOptions) -> Result<ResultSet> {
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
            Write::Insert { docs, replace } => {
                for mut d in docs {
                    schema.fill_generated(&mut d);
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
                for row in self.execute(&self.snapshot(), &select, &self.exec)?.rows {
                    let new = |f: usize, a: &Assign| match a {
                        Assign::Value(v) => v.clone(),
                        // NULL counts as zero.
                        Assign::Add(x) => match schema.fields[f].ty {
                            FieldType::Int => Value::Int(row[f].as_i64().unwrap_or(0) + *x as i64),
                            _ => Value::Float(row[f].as_f64().unwrap_or(0.0) + x),
                        },
                    };
                    if sets.iter().all(|(f, _)| in_place(*f)) {
                        for (f, a) in &sets {
                            t.set(&row[pk], &schema.fields[*f].name, new(*f, a))?;
                        }
                    } else {
                        let mut doc = row.clone();
                        for (f, a) in &sets {
                            doc[*f] = new(*f, a);
                        }
                        schema.fill_generated(&mut doc);
                        t.upsert(doc)?;
                    }
                    n += 1;
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
        let readers = self.readers(snap)?;
        Executor::new(snap, &readers, &self.exec).term_stats(select)
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
                    FieldType::Text { .. } => r.text(i).map(|_| ()),
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

fn with_source(mut opts: DbOptions) -> DbOptions {
    if opts.doc_source.is_none() {
        opts.doc_source = Some(Arc::new(StoredSource));
    }
    opts
}

/// Rebuilds documents for merges: stored fields from the doc store, vectors
/// and unstored structured fields from their indexes, generated text from
/// its sources.
pub struct StoredSource;

impl DocumentSource for StoredSource {
    fn documents(&self, schema: &Schema, seg: &SegmentFile, docs: &[DocId]) -> Result<Vec<Document>> {
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
