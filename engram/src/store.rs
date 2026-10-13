//! The memory table in a Strata file: appends, hybrid search, SQL.

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use strata::db::Db;
use strata::query::exec::ResultSet;
use strata::query::Params;
use strata::storage::{DbOptions, MergePolicy};
use strata::{Value, VectorIndexKind};

use crate::embed::Embedder;
use crate::record::{entity_id, file_entity_id, links_of, schema, Draft, Kind, Link, Record, COLUMNS};
use crate::{err, Result};

/// One search result with its component scores.
#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub record: Record,
    /// Raw BM25 score (None if not in the BM25 list).
    pub bm25: Option<f64>,
    /// Cosine similarity (None if not in the vector list).
    pub cos: Option<f64>,
    /// Final ranking score.
    pub score: f64,
}

/// What a search may return.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Exclude records of this session with `seq >= window_start` (the live window).
    pub exclude_window: Option<(String, i64)>,
    /// Only these kinds (empty: any).
    pub kinds: Vec<Kind>,
    /// Only records at or after this time.
    pub since_ts: Option<i64>,
    /// Only records before this time.
    pub until_ts: Option<i64>,
    /// Only this session.
    pub session: Option<String>,
}

impl Filter {
    fn sql(&self) -> String {
        let parts = self.conds();
        if parts.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", parts.join(" AND "))
        }
    }

    /// The filter's conditions. Entity records are never results unless
    /// asked for by kind.
    fn conds(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some((s, w)) = &self.exclude_window {
            parts.push(format!("NOT (session = '{}' AND seq >= {w})", quote(s)));
        }
        if !self.kinds.is_empty() {
            let ks: Vec<String> = self.kinds.iter().map(|k| format!("'{}'", k.as_str())).collect();
            parts.push(format!("kind IN ({})", ks.join(", ")));
        }
        if let Some(t) = self.since_ts {
            parts.push(format!("ts >= {t}"));
        }
        if let Some(t) = self.until_ts {
            parts.push(format!("ts < {t}"));
        }
        if let Some(s) = &self.session {
            parts.push(format!("session = '{}'", quote(s)));
        }
        if self.kinds.is_empty() {
            parts.push("kind != 'entity'".into());
        }
        parts
    }
}

/// How recall walks the links (see [`Store::reach`]).
#[derive(Clone, Debug)]
pub struct Reach {
    pub hops: usize,
    /// Kinds a walk may pass through (seeds and end points are exempt).
    pub through: Vec<Kind>,
    /// Stop after meeting this many records.
    pub max_nodes: usize,
}

fn quote(s: &str) -> String {
    s.replace('\'', "''")
}

#[derive(Clone, Debug)]
pub struct StoreOptions {
    pub vector_index: VectorIndexKind,
    /// Flush the memtable to a segment past this many records.
    pub flush_docs: usize,
    /// fsync every commit.
    pub sync: bool,
    /// Records per memtable chunk. The first query after a write rebuilds
    /// the last chunk's in-memory segment, so this bounds that work.
    pub mem_chunk_docs: usize,
    /// Memtable segments scan their vectors instead of building a graph.
    pub mem_flat: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions { vector_index: VectorIndexKind::Hnsw { m: 16, ef_construction: 100 }, flush_docs: 4096, sync: true, mem_chunk_docs: 256, mem_flat: true }
    }
}

pub struct Store {
    pub db: Db,
    pub embedder: Arc<dyn Embedder>,
    next_seq: AtomicI64,
    /// Strata takes one writer at a time and fails the others; this queues them.
    write: Mutex<()>,
    opts: StoreOptions,
}

impl Store {
    /// Opens the store at `path`, creating it if needed.
    pub fn open(path: &Path, embedder: Arc<dyn Embedder>, opts: StoreOptions) -> Result<Store> {
        let mut dbo = DbOptions::default();
        dbo.sync = opts.sync;
        dbo.memtable_max_docs = opts.flush_docs * 4;
        dbo.memtable_chunk_docs = opts.mem_chunk_docs.max(64);
        if opts.mem_flat {
            dbo.mem_build = Some(dbo.build.clone().set("vector.index", "flat"));
        }
        if path.exists() && Db::open(path, dbo.clone())?.schema().field("links").is_none() {
            migrate(path, &embedder, &opts)?;
        }
        let db = if path.exists() {
            Db::open(path, dbo)?
        } else {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            Db::create(path, schema(embedder.dim(), opts.vector_index.clone())?, dbo)?
        };
        let want = embedder.dim();
        if let Some(strata::FieldType::Vector { dim, .. }) = db.schema().field("emb").map(|f| f.ty.clone()) {
            if dim != want {
                return Err(err(format!("{} holds {dim}-dimension vectors; the embedder {} makes {want}", path.display(), embedder.name())));
            }
        }
        let store = Store { db, embedder, next_seq: AtomicI64::new(0), write: Mutex::new(()), opts };
        let last = store.query("SELECT seq FROM memory ORDER BY seq DESC LIMIT 1")?;
        let next = last.rows.first().and_then(|r| r[0].as_i64()).map_or(1, |s| s + 1);
        store.next_seq.store(next, Ordering::SeqCst);
        Ok(store)
    }

    /// Reserves `n` consecutive sequence numbers.
    pub fn reserve(&self, n: usize) -> i64 {
        self.next_seq.fetch_add(n as i64, Ordering::SeqCst)
    }

    pub fn last_seq(&self) -> i64 {
        self.next_seq.load(Ordering::SeqCst) - 1
    }

    pub fn len(&self) -> u64 {
        self.db.snapshot().n_live_docs()
    }

    /// Embeds and stores records whose sequence numbers are already assigned.
    pub fn insert(&self, records: &[Record]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let texts: Vec<String> = records.iter().map(|r| r.embed_text()).collect();
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let embs = self.embedder.embed(&refs);
        self.insert_embedded(records, embs)
    }

    /// Stores records with precomputed embeddings.
    pub fn insert_embedded(&self, records: &[Record], embs: Vec<Vec<f32>>) -> Result<()> {
        let schema = self.db.schema().clone();
        let _w = self.write.lock().unwrap();
        let mut txn = self.db.storage().begin()?;
        for (r, e) in records.iter().zip(embs) {
            let mut doc = r.to_doc(e);
            schema.fill_generated(&mut doc);
            txn.upsert(doc)?;
        }
        txn.commit()?;
        drop(_w);
        self.maintain()?;
        Ok(())
    }

    /// Bulk-loads records (for benchmarks and transcript back-fill).
    pub fn bulk_insert(&self, records: &[Record], embs: Vec<Vec<f32>>) -> Result<()> {
        let schema = self.db.schema().clone();
        let _w = self.write.lock().unwrap();
        let docs: Vec<Vec<Value>> = records
            .iter()
            .zip(embs)
            .map(|(r, e)| {
                let mut d = r.to_doc(e);
                schema.fill_generated(&mut d);
                d
            })
            .collect();
        self.db.storage().bulk_load(docs)?;
        Ok(())
    }

    /// Assigns sequence numbers and stores drafts for one session and turn.
    /// A draft's entities and files become links, and entities the store has
    /// not seen become records of their own.
    pub fn append(&self, session: &str, turn: i64, ts: i64, drafts: Vec<Draft>) -> Result<Vec<Record>> {
        let mut drafts = drafts;
        let mut names: Vec<(String, String)> = Vec::new();
        for d in &mut drafts {
            for n in std::mem::take(&mut d.entities) {
                if let Some(id) = entity_id(&n) {
                    if !d.links.iter().any(|l| l.to == id) {
                        d.links.push(Link::new("about", id.clone()));
                    }
                    names.push((id, n.trim().to_string()));
                }
            }
            if links_files(d.kind) {
                for f in &d.files {
                    let id = file_entity_id(f);
                    if !d.links.iter().any(|l| l.to == id) {
                        d.links.push(Link::new("touches", id.clone()));
                    }
                    names.push((id, f.clone()));
                }
            }
        }
        let first = self.reserve(drafts.len());
        let mut records: Vec<Record> = drafts.into_iter().enumerate().map(|(i, d)| Record::from_draft(d, session, turn, first + i as i64, ts)).collect();
        let entities = self.new_entities(names, ts)?;
        let out = records.clone();
        records.extend(entities);
        self.insert(&records)?;
        Ok(out)
    }

    /// Entity records for the names not stored yet.
    fn new_entities(&self, names: Vec<(String, String)>, ts: i64) -> Result<Vec<Record>> {
        let mut seen = std::collections::HashSet::new();
        let names: Vec<(String, String)> = names.into_iter().filter(|(id, _)| seen.insert(id.clone())).collect();
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = names.iter().map(|(id, _)| id.clone()).collect();
        let have: std::collections::HashSet<String> = self.get(&ids)?.into_iter().map(|r| r.id).collect();
        let new: Vec<(String, String)> = names.into_iter().filter(|(id, _)| !have.contains(id)).collect();
        let first = self.reserve(new.len());
        Ok(new
            .into_iter()
            .enumerate()
            .map(|(i, (id, name))| {
                let mut r = Record::from_draft(Draft::new(Kind::Entity, name), "-", 0, first + i as i64, ts);
                r.id = id;
                r
            })
            .collect())
    }

    /// Builds the current snapshot's in-memory segments and readers now, so
    /// the next query does not pay for it.
    pub fn warm(&self) {
        let snap = self.db.snapshot();
        let _ = self.db.readers(&snap);
    }

    /// Flushes the memtable when it is large, then merges small segments.
    pub fn maintain(&self) -> Result<()> {
        let stats = self.db.storage().file_stats();
        if stats.memtable_docs >= self.opts.flush_docs {
            let _w = self.write.lock().unwrap();
            self.db.storage().flush()?;
            self.db.storage().merge(&MergePolicy::default())?;
        }
        Ok(())
    }

    /// Writes everything to segments (on shutdown).
    pub fn flush(&self) -> Result<()> {
        let _w = self.write.lock().unwrap();
        self.db.storage().flush()?;
        Ok(())
    }

    /// Changes a record's tag (e.g. planned work that is now done).
    pub fn retag(&self, id: &str, tag: &str) -> Result<bool> {
        let Some(mut r) = self.get(&[id.to_string()])?.pop() else { return Ok(false) };
        r.tag = tag.to_string();
        self.insert(&[r])?;
        Ok(true)
    }

    /// Deletes a record.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let _w = self.write.lock().unwrap();
        let mut txn = self.db.storage().begin()?;
        let found = txn.delete(&Value::Str(id.to_string()))?;
        txn.commit()?;
        Ok(found)
    }

    /// Runs SQL against the memory table.
    pub fn query(&self, sql: &str) -> Result<ResultSet> {
        Ok(self.db.query(sql, &Params::new())?)
    }

    pub fn query_with(&self, sql: &str, params: &Params) -> Result<ResultSet> {
        Ok(self.db.query(sql, params)?)
    }

    /// Records by id (any order).
    pub fn get(&self, ids: &[String]) -> Result<Vec<Record>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let list: Vec<String> = ids.iter().map(|i| format!("'{}'", quote(i))).collect();
        let rs = self.query(&format!("SELECT {COLUMNS} FROM memory WHERE id IN ({}) LIMIT {}", list.join(", "), ids.len()))?;
        Ok(rs.rows.iter().filter_map(|r| row_record(r)).collect())
    }

    /// The records of `session` with `seq` in `[lo, hi]`, in order.
    pub fn range(&self, session: &str, lo: i64, hi: i64) -> Result<Vec<Record>> {
        let rs = self.query(&format!(
            "SELECT {COLUMNS} FROM memory WHERE session = '{}' AND seq BETWEEN {lo} AND {hi} ORDER BY seq ASC LIMIT {}",
            quote(session),
            (hi - lo + 1).max(1)
        ))?;
        Ok(rs.rows.iter().filter_map(|r| row_record(r)).collect())
    }

    /// The newest records of the given kinds.
    pub fn latest(&self, kinds: &[Kind], limit: usize, filter: &Filter) -> Result<Vec<Record>> {
        let mut f = filter.clone();
        f.kinds = kinds.to_vec();
        let rs = self.query(&format!("SELECT {COLUMNS} FROM memory {} ORDER BY seq DESC LIMIT {limit}", f.sql()))?;
        Ok(rs.rows.iter().filter_map(|r| row_record(r)).collect())
    }

    /// The top `n` by BM25 over `body`, with raw scores.
    pub fn bm25_top(&self, query: &str, n: usize, filter: &Filter) -> Result<Vec<(Record, f64)>> {
        let sql = format!("SELECT {COLUMNS}, score() FROM memory {} ORDER BY bm25(body, :q) DESC LIMIT {n}", filter.sql());
        let rs = self.query_with(&sql, &Params::new().set("q", Value::Str(query.to_string())))?;
        Ok(rs.rows.iter().filter_map(|r| Some((row_record(r)?, r.last()?.as_f64()?))).collect())
    }

    /// The records reachable from `seeds` over links within `r.hops`, both
    /// ways, passing only through the kinds `r.through` allows, best
    /// `n` by cosine similarity to `v`. Seeds are left out.
    pub fn reach(&self, seeds: &[String], v: &[f32], n: usize, filter: &Filter, r: &Reach) -> Result<Vec<(Record, f64)>> {
        if seeds.is_empty() || r.hops == 0 {
            return Ok(Vec::new());
        }
        let list: Vec<String> = seeds.iter().map(|i| format!("'{}'", quote(i))).collect();
        let through = if r.through.is_empty() {
            String::new()
        } else {
            let ks: Vec<String> = r.through.iter().map(|k| format!("'{}'", k.as_str())).collect();
            format!(", through => kind IN ({})", ks.join(", "))
        };
        let mut conds = filter.conds();
        conds.push(format!("REACHABLE(links, ({}), hops => {}, dir => 'both', max_nodes => {}{through})", list.join(", "), r.hops, r.max_nodes));
        let sql = format!("SELECT {COLUMNS}, score() FROM memory WHERE {} ORDER BY similarity(emb, :v) DESC LIMIT {n}", conds.join(" AND "));
        let rs = self.query_with(&sql, &Params::new().set("v", Value::Vector(v.to_vec().into())))?;
        let seeds: std::collections::HashSet<&str> = seeds.iter().map(|s| s.as_str()).collect();
        Ok(rs.rows.iter().filter_map(|r| Some((row_record(r)?, r.last()?.as_f64()?))).filter(|(r, _)| !seeds.contains(r.id.as_str())).collect())
    }

    /// The top `n` by cosine similarity to `v`.
    pub fn vector_top(&self, v: &[f32], n: usize, filter: &Filter) -> Result<Vec<(Record, f64)>> {
        let sql = format!("SELECT {COLUMNS}, score() FROM memory {} ORDER BY similarity(emb, :v) DESC LIMIT {n}", filter.sql());
        let rs = self.query_with(&sql, &Params::new().set("v", Value::Vector(v.to_vec().into())))?;
        Ok(rs.rows.iter().filter_map(|r| Some((row_record(r)?, r.last()?.as_f64()?))).collect())
    }

    /// Inverse document frequency of each analysed query term over `body`,
    /// from the segments' term dictionaries (deleted records still count).
    pub fn idf(&self, terms: &[String]) -> Result<Vec<f64>> {
        let snap = self.db.snapshot();
        let readers = self.db.readers(&snap)?;
        let field = self.db.schema().field_index("body").ok_or_else(|| err("no body field"))?;
        let mut n = 0u64;
        let mut df = vec![0u64; terms.len()];
        for r in &readers {
            n += r.n_docs as u64;
            if let Some(t) = r.text(field)? {
                for (d, term) in df.iter_mut().zip(terms) {
                    *d += t.doc_freq(term) as u64;
                }
            }
        }
        let n = n.max(1) as f64;
        Ok(df.into_iter().map(|d| (1.0 + (n - d as f64 + 0.5) / (d as f64 + 0.5)).ln()).collect())
    }
}

/// A row projected with [`COLUMNS`] as a record.
pub fn row_record(r: &[Value]) -> Option<Record> {
    let s = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let n = |i: usize| r.get(i).and_then(|v| v.as_i64()).unwrap_or(0);
    Some(Record {
        id: s(0),
        session: s(1),
        kind: Kind::parse(r.get(2)?.as_str()?)?,
        tag: s(3),
        turn: n(4),
        seq: n(5),
        ts: n(6),
        importance: r.get(7).and_then(|v| v.as_f64()).unwrap_or(0.0),
        files: s(8),
        text: s(9),
        ctx: s(10),
        links: r.get(11).map(links_of).unwrap_or_default(),
    })
}

/// Kinds whose files become links: what was decided or went wrong about a
/// file, not every read of it.
fn links_files(k: Kind) -> bool {
    k.is_knowledge() || matches!(k, Kind::Error | Kind::Episode)
}

/// Rewrites a store made before records had links into the current schema
/// (vectors are not stored, so the records are embedded again). The old
/// file is kept next to it as `<name>.v1`.
fn migrate(path: &Path, embedder: &Arc<dyn Embedder>, opts: &StoreOptions) -> Result<()> {
    let old = Db::open(path, DbOptions::default())?;
    let cols = "id, session, kind, tag, turn, seq, ts, importance, files, text, ctx";
    let rs = old.query(&format!("SELECT {cols} FROM memory ORDER BY seq ASC LIMIT 1000000000"), &Params::new())?;
    let records: Vec<Record> = rs.rows.iter().filter_map(|r| row_record(r)).collect();
    drop(old);
    let tmp = path.with_extension("migrating");
    let side = |p: &Path, suffix: &str| {
        let mut s = p.as_os_str().to_owned();
        s.push(suffix);
        std::path::PathBuf::from(s)
    };
    for p in [tmp.clone(), side(&tmp, "-wal")] {
        let _ = std::fs::remove_file(p);
    }
    {
        let new = Store::open(&tmp, embedder.clone(), StoreOptions { sync: false, ..opts.clone() })?;
        for chunk in records.chunks(4096) {
            let texts: Vec<String> = chunk.iter().map(|r| r.embed_text()).collect();
            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
            new.bulk_insert(chunk, embedder.embed(&refs))?;
        }
        new.flush()?;
    }
    let bak = path.with_extension("v1");
    std::fs::rename(path, &bak)?;
    let _ = std::fs::rename(side(path, "-wal"), side(&bak, "-wal"));
    std::fs::rename(&tmp, path)?;
    let _ = std::fs::rename(side(&tmp, "-wal"), side(path, "-wal"));
    eprintln!("engram: rewrote {} records of {} with links (the old file is {})", records.len(), path.display(), bak.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;

    fn store(dir: &Path) -> Store {
        Store::open(&dir.join("m.strata"), Arc::new(HashEmbedder { dim: 64 }), StoreOptions { sync: false, ..Default::default() }).unwrap()
    }

    #[test]
    fn append_search_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.append("sess-a", 1, 100, vec![Draft::new(Kind::Prompt, "Use BM25 with k1 1.2 for the ranking")]).unwrap();
        s.append("sess-a", 1, 101, vec![Draft::new(Kind::Answer, "The planner probes local selectivity").ctx("about the planner")]).unwrap();
        s.append("sess-b", 1, 200, vec![Draft::new(Kind::Decision, "We decided the vector index is HNSW")]).unwrap();
        assert_eq!(s.len(), 3);
        let hits = s.bm25_top("ranking bm25", 10, &Filter::default()).unwrap();
        assert_eq!(hits[0].0.kind, Kind::Prompt);
        // Context is searchable.
        assert_eq!(s.bm25_top("planner", 10, &Filter::default()).unwrap().len(), 1);
        // The live window of sess-a (seq >= 2) is excluded.
        let f = Filter { exclude_window: Some(("sess-a".into(), 2)), ..Default::default() };
        assert!(s.bm25_top("planner", 10, &f).unwrap().is_empty());
        let v = s.embedder.embed_query("vector index HNSW");
        assert_eq!(s.vector_top(&v, 1, &Filter::default()).unwrap()[0].0.kind, Kind::Decision);
        let idf = s.idf(&["planner".into(), "zzz".into()]).unwrap();
        assert!(idf[1] > idf[0]);
        drop(s);
        let s = store(dir.path());
        assert_eq!(s.last_seq(), 3);
        assert_eq!(s.range("sess-a", 1, 2).unwrap().len(), 2);
        assert_eq!(s.latest(&[Kind::Decision], 5, &Filter::default()).unwrap().len(), 1);
        assert!(s.delete("sessb:3").unwrap());
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn entities_link_records_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let id = |v: Vec<Record>| v[0].id.clone();
        let prompt = id(s.append("sess-a", 1, 100, vec![Draft::new(Kind::Prompt, "Build the count sheet in walking order")]).unwrap());
        let todo = id(s.append("sess-a", 1, 101, vec![Draft::new(Kind::Todo, "The count sheet is sorted by bin location").entities(vec!["Count sheet".into(), "bins".into()]).link("from", prompt.clone())]).unwrap());
        let fact_b = id(s.append("sess-b", 1, 200, vec![Draft::new(Kind::Fact, "The auditor wants a blind count sheet").entities(vec!["count sheets".into(), "auditor".into()])]).unwrap());
        let fact_c = id(s.append("sess-c", 1, 300, vec![Draft::new(Kind::Fact, "Prices are stored in integer cents").entities(vec!["prices".into()])]).unwrap());
        // One prompt, three notes and four entities (count sheet, bin,
        // auditor, price): the plural meets the singular.
        assert_eq!(s.len(), 8);
        assert_eq!(s.get(&["e:count sheet".into()]).unwrap()[0].text, "Count sheet");
        // Entities are not search results.
        assert!(s.bm25_top("count sheet", 10, &Filter::default()).unwrap().iter().all(|(r, _)| r.kind != Kind::Entity));
        // From the first note: its source turn (1 hop) and, through the
        // shared entity, the other session's note (2 hops); not the prices.
        let v = s.embedder.embed_query("count sheet");
        let r = Reach { hops: 2, through: crate::recall::THROUGH.to_vec(), max_nodes: 100 };
        let got: Vec<String> = s.reach(&[todo.clone()], &v, 10, &Filter::default(), &r).unwrap().into_iter().map(|(r, _)| r.id).collect();
        assert!(got.contains(&prompt) && got.contains(&fact_b), "{got:?}");
        assert!(!got.iter().any(|i| i.starts_with("e:") || *i == fact_c || *i == todo), "{got:?}");
        // A retag keeps the links.
        s.retag(&todo, "done").unwrap();
        assert_eq!(s.get(&[todo]).unwrap()[0].links.len(), 3);
    }

    #[test]
    fn stores_without_links_are_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.strata");
        let mut fields = schema(64, VectorIndexKind::Flat).unwrap().fields;
        fields.retain(|f| f.name != "links");
        let old = strata::Schema::new("memory", fields, Some("id")).unwrap();
        {
            let db = Db::create(&path, old.clone(), DbOptions::default()).unwrap();
            let mut txn = db.storage().begin().unwrap();
            let r = Record::from_draft(Draft::new(Kind::Decision, "We use HNSW for the vector index"), "sess-a", 1, 1, 100);
            let mut doc = r.to_doc(vec![0.1; 64]);
            doc.pop();
            old.fill_generated(&mut doc);
            txn.upsert(doc).unwrap();
            txn.commit().unwrap();
            db.storage().flush().unwrap();
        }
        let s = store(dir.path());
        assert!(s.db.schema().field("links").is_some());
        assert_eq!(s.len(), 1);
        assert_eq!(s.latest(&[Kind::Decision], 5, &Filter::default()).unwrap()[0].text, "We use HNSW for the vector index");
        assert!(dir.path().join("m.v1").exists());
    }
}
