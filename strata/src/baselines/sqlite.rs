//! SQLite baseline: one database file holding the documents in a rowid
//! table, B-tree indexes on the structured fields, an FTS5 index over the
//! body, and vectors as float32 blobs searched by brute force.
//!
//! ```sql
//! CREATE TABLE docs(id TEXT PRIMARY KEY, title TEXT, text TEXT, year INTEGER,
//!                   category TEXT, rating REAL, topic TEXT, emb BLOB);
//! CREATE INDEX docs_year ON docs(year);   -- likewise category, rating, topic
//! CREATE VIRTUAL TABLE fts USING fts5(body, content='', contentless_delete=1,
//!                                     tokenize='porter unicode61');
//! ```
//!
//! * **FTS5** is contentless with `rowid = docs.rowid`. The body (title +
//!   text) exists only as two `docs` columns, so an external-content table
//!   would need a stored body column or a view; a contentless table stores
//!   just the index and the document lengths that `bm25()` needs, and
//!   `contentless_delete=1` keeps it updatable. The tokenizer is
//!   `porter unicode61` (Unicode word split, case and diacritic folding,
//!   Porter stemmer); there is no stopword list.
//! * **BM25** is FTS5's `bm25()` (k1 = 1.2, b = 0.75 are fixed in FTS5), via
//!   the `rank` column, over the OR of the quoted query terms
//!   ([`workload::query_terms`]). Boolean queries use FTS5's `AND`/`OR`.
//!   Filtered BM25 joins `fts` with `docs`.
//! * **Vectors** are little-endian float32 blobs in `docs.emb`, ranked by
//!   `vdot(emb, ?)`, a scalar function registered on the connection, in a
//!   full scan (`ORDER BY vdot(emb, ?) DESC LIMIT k`): what SQLite offers
//!   without an extension. (The sqlite-vec extension's `vec0` virtual table
//!   is the alternative; it is also a brute-force scan.)
//! * **Hybrid** is RRF in the application over a BM25 and a vector query.
//! * **Load**: one transaction in WAL mode (rows and FTS5 postings, then the
//!   B-tree indexes, then FTS5 `optimize`, which merges the index into one
//!   segment), then `ANALYZE` and a truncating checkpoint, so the database
//!   file holds everything.
//! * **Settings** are SQLite's defaults: 4096-byte pages, `cache_size`
//!   -2000 (2000 KiB of page cache per connection), no memory mapping,
//!   `synchronous = FULL`. Queries use prepared statements on one
//!   connection.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection};

use super::file_size;
use crate::data::{col, Collection};
use crate::error::{Error, Result};
use crate::workload::run::{BuildReport, QueryOutput, System};
use crate::workload::{self, Kind, Pred, WorkloadQuery, FACET_K, RRF_DEPTH, TOP_K};

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Query(format!("sqlite: {e}"))
    }
}

const SCHEMA: &str = "
CREATE TABLE docs(id TEXT PRIMARY KEY, title TEXT, text TEXT, year INTEGER,
                  category TEXT, rating REAL, topic TEXT, emb BLOB);
CREATE VIRTUAL TABLE fts USING fts5(body, content='', contentless_delete=1,
                                    tokenize='porter unicode61');
";

const INDEXES: &str = "
CREATE INDEX docs_year ON docs(year);
CREATE INDEX docs_category ON docs(category);
CREATE INDEX docs_rating ON docs(rating);
CREATE INDEX docs_topic ON docs(topic);
";

/// What a statement contributes to a query's output.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Role {
    /// A ranking of ids (two rankings are fused by RRF).
    Ids,
    Count,
    Facets,
}

/// One SQL statement of a workload query.
struct Statement {
    role: Role,
    sql: String,
    params: Vec<SqlValue>,
}

/// The SQLite baseline (see the module docs).
pub struct Sqlite {
    path: PathBuf,
    /// `None` only while [`System::evict_caches`] reopens the database.
    conn: Option<Connection>,
}

impl Sqlite {
    /// Loads `coll` into a new database at `path` (replacing any).
    pub fn build(path: &Path, coll: &Collection) -> Result<(Sqlite, BuildReport)> {
        for p in Sqlite::paths(path) {
            if p.exists() {
                std::fs::remove_file(&p)?;
            }
        }
        let start = Instant::now();
        let mut phases: Vec<(String, f64)> = Vec::new();
        let mut lap = Instant::now();
        let mut phase = |name: &str| {
            phases.push((name.to_string(), lap.elapsed().as_secs_f64()));
            lap = Instant::now();
        };
        let conn = Connection::open(path)?;
        let options = compile_options(&conn)?;
        if !options.iter().any(|o| o == "ENABLE_FTS5") {
            return Err(Error::Query("sqlite: built without FTS5".into()));
        }
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch("BEGIN")?;
        {
            let mut docs = conn.prepare(
                "INSERT INTO docs(rowid, id, title, text, year, category, rating, topic, emb)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            let mut fts = conn.prepare("INSERT INTO fts(rowid, body) VALUES (?, ?)")?;
            let mut blob = Vec::with_capacity(coll.dim * 4);
            for (i, d) in coll.docs.iter().enumerate() {
                let rowid = i as i64 + 1;
                blob.clear();
                for x in d[col::EMB].as_vector().unwrap_or(&[]) {
                    blob.extend_from_slice(&x.to_le_bytes());
                }
                docs.execute(params![
                    rowid,
                    d[col::ID].as_str(),
                    d[col::TITLE].as_str(),
                    d[col::TEXT].as_str(),
                    d[col::YEAR].as_i64(),
                    d[col::CATEGORY].as_str(),
                    d[col::RATING].as_f64(),
                    d[col::TOPIC].as_str(),
                    &blob,
                ])?;
                fts.execute(params![rowid, d[col::BODY].as_str()])?;
            }
        }
        phase("insert");
        conn.execute_batch(INDEXES)?;
        phase("indexes");
        conn.execute_batch("INSERT INTO fts(fts) VALUES('optimize')")?;
        phase("fts_optimize");
        conn.execute_batch("COMMIT")?;
        phase("commit");
        conn.execute_batch("ANALYZE")?;
        phase("analyze");
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        phase("checkpoint");
        let seconds = start.elapsed().as_secs_f64();

        // Space per table and index (dbstat), plus the raw vector bytes, which
        // are part of `docs` (mostly its overflow pages).
        let mut components: Vec<(String, u64)> = conn
            .prepare("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name ORDER BY 2 DESC")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<_>>()?;
        components.push(("emb_bytes_within_docs".into(), (coll.docs.len() * coll.dim * 4) as u64));
        drop(conn);
        let sys = Sqlite::open(path)?;
        let bytes = sys.files().iter().map(|p| file_size(p)).sum();
        Ok((sys, BuildReport { seconds, phases, bytes, components }))
    }

    /// Opens an existing database.
    pub fn open(path: &Path) -> Result<Sqlite> {
        let conn = Connection::open(path)?;
        conn.create_scalar_function("vdot", 2, FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC, vdot)?;
        conn.set_prepared_statement_cache_capacity(128);
        // Read the schema now, as an application does at startup, so that a
        // cold query measures only its own reads.
        conn.query_row("SELECT count(*) FROM docs WHERE rowid = 0", [], |_| Ok(()))?;
        Ok(Sqlite { path: path.to_path_buf(), conn: Some(conn) })
    }

    /// The database file and its WAL and shared-memory sidecars.
    fn paths(path: &Path) -> [PathBuf; 3] {
        let side = |s: &str| PathBuf::from(format!("{}{s}", path.display()));
        [path.to_path_buf(), side("-wal"), side("-shm")]
    }

    /// Removes the database files.
    pub fn remove(path: &Path) -> Result<()> {
        for p in Sqlite::paths(path) {
            if p.exists() {
                std::fs::remove_file(p)?;
            }
        }
        Ok(())
    }

    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("connection is open")
    }

    /// SQLite library version.
    pub fn version() -> String {
        format!("SQLite {} (rusqlite, bundled)", rusqlite::version())
    }

    /// The library's compile-time options (`PRAGMA compile_options`).
    pub fn compile_options(&self) -> Result<Vec<String>> {
        compile_options(self.conn())
    }

    /// The statements that answer `q`.
    fn statements(&self, q: &WorkloadQuery, coll: &Collection) -> Vec<Statement> {
        let text = || workload::query_terms(q.text(coll));
        let vector = || q.query.map(|i| vector_blob(&coll.queries[i].vector));
        match q.kind {
            Kind::Bm25 | Kind::FilteredBm25 => bm25_statement(&text(), &q.filter, TOP_K).into_iter().collect(),
            Kind::And2 | Kind::And3 | Kind::Or3 => {
                let op = if q.kind == Kind::Or3 { " OR " } else { " AND " };
                let m = fts_query(&q.words, op);
                vec![
                    Statement { role: Role::Count, sql: "SELECT count(*) FROM fts WHERE fts MATCH ?".into(), params: vec![m.clone().into()] },
                    Statement {
                        role: Role::Ids,
                        sql: format!(
                            "SELECT docs.id FROM (SELECT rowid AS r FROM fts WHERE fts MATCH ? ORDER BY rowid LIMIT {TOP_K}) AS m
                             JOIN docs ON docs.rowid = m.r ORDER BY m.r"
                        ),
                        params: vec![m.into()],
                    },
                ]
            }
            Kind::Filter => {
                let mut params = Vec::new();
                let c = where_clause(&q.filter, &mut params);
                vec![
                    Statement { role: Role::Count, sql: format!("SELECT count(*) FROM docs WHERE {c}"), params: params.clone() },
                    Statement {
                        role: Role::Ids,
                        sql: format!("SELECT id FROM docs WHERE {c} ORDER BY rating DESC, id LIMIT {TOP_K}"),
                        params,
                    },
                ]
            }
            Kind::Vector | Kind::FilteredVector => vector().map(|v| vector_statement(v, &q.filter, TOP_K)).into_iter().collect(),
            Kind::Hybrid | Kind::FilteredHybrid => {
                let mut s: Vec<Statement> = bm25_statement(&text(), &q.filter, RRF_DEPTH).into_iter().collect();
                s.extend(vector().map(|v| vector_statement(v, &q.filter, RRF_DEPTH)));
                s
            }
            Kind::Facet => vec![Statement {
                role: Role::Facets,
                sql: format!(
                    "SELECT docs.category, count(*) FROM fts JOIN docs ON docs.rowid = fts.rowid
                     WHERE fts MATCH ? GROUP BY docs.category ORDER BY 2 DESC, 1 LIMIT {FACET_K}"
                ),
                params: vec![fts_query(&q.words, " AND ").into()],
            }],
        }
    }

    /// `EXPLAIN QUERY PLAN` of each statement of `q`: (SQL, plan lines).
    pub fn explain(&self, q: &WorkloadQuery, coll: &Collection) -> Result<Vec<(String, Vec<String>)>> {
        let mut out = Vec::new();
        for s in self.statements(q, coll) {
            let mut st = self.conn().prepare(&format!("EXPLAIN QUERY PLAN {}", s.sql))?;
            let lines = st.query_map(params_from_iter(&s.params), |r| r.get::<_, String>(3))?.collect::<rusqlite::Result<_>>()?;
            out.push((squeeze(&s.sql), lines));
        }
        Ok(out)
    }
}

impl System for Sqlite {
    fn name(&self) -> &str {
        "sqlite"
    }

    fn supports(&self, _kind: Kind) -> bool {
        true
    }

    fn run(&mut self, q: &WorkloadQuery, coll: &Collection) -> Result<QueryOutput> {
        let conn = self.conn();
        let mut out = QueryOutput::default();
        let mut rankings: Vec<Vec<String>> = Vec::new();
        for s in self.statements(q, coll) {
            let mut st = conn.prepare_cached(&s.sql)?;
            let params = params_from_iter(&s.params);
            match s.role {
                Role::Ids => rankings.push(st.query_map(params, |r| r.get(0))?.collect::<rusqlite::Result<_>>()?),
                Role::Count => out.count = Some(st.query_row(params, |r| r.get::<_, i64>(0))? as u64),
                Role::Facets => {
                    let rows = st.query_map(params, |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
                    out.facets = Some(rows.collect::<rusqlite::Result<_>>()?);
                }
            }
        }
        out.ids = if rankings.len() > 1 {
            let refs: Vec<&[String]> = rankings.iter().map(Vec::as_slice).collect();
            workload::rrf(&refs, TOP_K)
        } else {
            rankings.pop().unwrap_or_default()
        };
        Ok(out)
    }

    fn evict_caches(&mut self) -> Result<()> {
        if let Some(conn) = self.conn.take() {
            conn.close().map_err(|(_, e)| e)?;
        }
        for p in self.files() {
            crate::iostat::evict(&p)?;
        }
        *self = Sqlite::open(&self.path)?;
        Ok(())
    }

    fn files(&self) -> Vec<PathBuf> {
        // The -shm file is shared memory, not data, and is excluded.
        Sqlite::paths(&self.path).into_iter().take(2).filter(|p| p.exists()).collect()
    }
}

fn compile_options(conn: &Connection) -> Result<Vec<String>> {
    let mut st = conn.prepare("PRAGMA compile_options")?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// An FTS5 query joining quoted terms with `op` (" AND " or " OR ").
fn fts_query(terms: &[String], op: &str) -> String {
    terms.iter().map(|t| format!("\"{}\"", t.replace('"', "\"\""))).collect::<Vec<_>>().join(op)
}

/// `docs.`-qualified SQL for a filter, appending its parameters.
fn where_clause(filter: &[Pred], params: &mut Vec<SqlValue>) -> String {
    let in_list = |field: &str, values: &[String], params: &mut Vec<SqlValue>| {
        params.extend(values.iter().map(|v| SqlValue::Text(v.clone())));
        if values.len() == 1 {
            format!("docs.{field} = ?")
        } else {
            format!("docs.{field} IN ({})", vec!["?"; values.len()].join(", "))
        }
    };
    filter
        .iter()
        .map(|p| match p {
            Pred::Year(lo, hi) => {
                params.extend([SqlValue::Integer(*lo), SqlValue::Integer(*hi)]);
                "docs.year BETWEEN ? AND ?".to_string()
            }
            Pred::Rating(lo, hi) => {
                params.extend([SqlValue::Real(*lo), SqlValue::Real(*hi)]);
                "docs.rating BETWEEN ? AND ?".to_string()
            }
            Pred::Category(v) => in_list("category", v, params),
            Pred::Topic(v) => in_list("topic", v, params),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Top-`k` BM25 over the OR of `terms`, within `filter`; `None` without
/// terms.
fn bm25_statement(terms: &[String], filter: &[Pred], k: usize) -> Option<Statement> {
    if terms.is_empty() {
        return None;
    }
    let mut params = vec![SqlValue::Text(fts_query(terms, " OR "))];
    let sql = if filter.is_empty() {
        // Rank inside FTS5 first, then look up the k ids.
        format!(
            "SELECT docs.id FROM (SELECT rowid AS r, rank AS s FROM fts WHERE fts MATCH ? ORDER BY rank LIMIT {k}) AS m
             JOIN docs ON docs.rowid = m.r ORDER BY m.s"
        )
    } else {
        let c = where_clause(filter, &mut params);
        format!("SELECT docs.id FROM fts JOIN docs ON docs.rowid = fts.rowid WHERE fts MATCH ? AND {c} ORDER BY fts.rank LIMIT {k}")
    };
    Some(Statement { role: Role::Ids, sql, params })
}

/// Exact top-`k` by dot product within `filter`.
fn vector_statement(blob: Vec<u8>, filter: &[Pred], k: usize) -> Statement {
    let mut params = Vec::new();
    let wh = if filter.is_empty() { String::new() } else { format!(" WHERE {}", where_clause(filter, &mut params)) };
    params.push(SqlValue::Blob(blob));
    Statement { role: Role::Ids, sql: format!("SELECT id FROM docs{wh} ORDER BY vdot(emb, ?) DESC LIMIT {k}"), params }
}

fn vector_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Collapses whitespace runs, for printing SQL on one line.
fn squeeze(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `vdot(a, b)`: dot product of two little-endian float32 blobs.
fn vdot(ctx: &Context<'_>) -> rusqlite::Result<f64> {
    let blob = |i| ctx.get_raw(i).as_blob().map_err(|e| rusqlite::Error::UserFunctionError(e.into()));
    let (a, b) = (blob(0)?, blob(1)?);
    if a.len() != b.len() || a.len() % 4 != 0 {
        return Err(rusqlite::Error::UserFunctionError("vdot: vectors of different dimensions".into()));
    }
    Ok(dot_le(a, b) as f64)
}

/// Dot product of float32 arrays given as little-endian bytes. Sixteen
/// independent accumulators let the compiler vectorise the loop (a single
/// running sum would have to be computed in order).
fn dot_le(a: &[u8], b: &[u8]) -> f32 {
    const LANES: usize = 16;
    let f = |x: &[u8], j: usize| f32::from_le_bytes([x[4 * j], x[4 * j + 1], x[4 * j + 2], x[4 * j + 3]]);
    let mut acc = [0f32; LANES];
    let (ca, cb) = (a.chunks_exact(4 * LANES), b.chunks_exact(4 * LANES));
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        for (j, a) in acc.iter_mut().enumerate() {
            *a += f(x, j) * f(y, j);
        }
    }
    let tail: f32 = (0..ra.len() / 4).map(|j| f(ra, j) * f(rb, j)).sum();
    acc.iter().sum::<f32>() + tail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_matches_naive() {
        let a: Vec<f32> = (0..1027).map(|i| (i as f32 * 0.37).sin()).collect();
        let b: Vec<f32> = (0..1027).map(|i| (i as f32 * 0.11).cos()).collect();
        let naive: f64 = a.iter().zip(&b).map(|(x, y)| (*x as f64) * (*y as f64)).sum();
        assert!((dot_le(&vector_blob(&a), &vector_blob(&b)) as f64 - naive).abs() < 1e-3);
    }

    #[test]
    fn fts_queries_quote_terms() {
        let t = vec!["covid".to_string(), "say\"x".to_string()];
        assert_eq!(fts_query(&t, " OR "), "\"covid\" OR \"say\"\"x\"");
    }
}
