//! Reading a SQLite file's tables into Strata tables ([`super`]).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value as Json};

use super::{distinct, quote, META_TABLE};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::ingest::{self, IMPORT_BATCH};
use crate::schema::{Analyzer, Field, FieldType, Logical, Schema, DEFAULT_HNSW};
use crate::storage::DbOptions;
use crate::value::{Document, Value};

/// How an import goes.
#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    /// The SQLite tables to import (any case); every table when empty.
    pub tables: Vec<String>,
    /// Schemas for the Strata tables the import creates, in place of the
    /// ones it would make from the SQLite tables.
    pub schemas: Vec<Given>,
    /// Keep BLOB columns as base64 text, in a stored KEYWORD field that is
    /// not indexed, rather than leave them out.
    pub blobs_base64: bool,
}

/// A schema given for a table an import creates
/// ([`ImportOptions::schemas`]): it applies to the SQLite table `source`
/// names, or else to the one whose name (as Strata spells it) it has. The
/// SQLite columns fill the fields of their names.
#[derive(Clone, Debug)]
pub struct Given {
    pub source: Option<String>,
    pub schema: Schema,
}

/// Schemas for [`ImportOptions::schemas`] from JSON: a schema in either
/// form, an array of them, or a plan ([`Plan::to_json`], edited or not),
/// whose tables are short-form schemas that name their `source`.
pub fn schemas_from_json(text: &str) -> Result<Vec<Given>> {
    let j: Json = serde_json::from_str(text).map_err(|e| Error::Schema(format!("schema JSON: {e}")))?;
    let items = match j {
        Json::Array(xs) => xs,
        Json::Object(mut o) if o.get("tables").is_some_and(Json::is_array) => match o.remove("tables") {
            Some(Json::Array(xs)) => xs,
            _ => unreachable!("checked to be an array"),
        },
        o => vec![o],
    };
    items.iter().map(|s| Ok(Given { source: s.get("source").and_then(Json::as_str).map(String::from), schema: Schema::from_json_value(s)? })).collect()
}

/// What an import does with one SQLite table.
#[derive(Clone, Debug)]
pub struct TablePlan {
    /// The SQLite table.
    pub source: String,
    /// The Strata table's schema: the one the import creates, or the
    /// existing table's.
    pub schema: Schema,
    /// Whether the Strata table existed when the plan was made. Rows go
    /// into an empty table in one bulk load, and are otherwise upserted by
    /// key (inserted, in a table without one).
    pub exists: bool,
    /// The SQLite columns read, each with the field it fills.
    pub columns: Vec<(String, usize)>,
    /// What does not carry over, and names Strata spells otherwise.
    pub notes: Vec<String>,
    /// How each column's values are read, by position in `columns`.
    reads: Vec<Read>,
}

/// How a column's values become a field's.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Read {
    /// As the field's type takes them; integers and reals in a TIMESTAMP
    /// field as Unix time in units of this many microseconds (reals under
    /// 10^7 as Julian days).
    Plain { time_unit: i64 },
    /// BLOB values as base64 text.
    Base64,
}

const SECONDS: Read = Read::Plain { time_unit: 1_000_000 };

/// What an import would do ([`plan`]).
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// The tables imported, in the order they are created: a table before
    /// the tables that reference it.
    pub tables: Vec<TablePlan>,
    /// The SQLite tables and views left out, each with the reason.
    pub skipped: Vec<(String, String)>,
}

impl Plan {
    /// The plan as JSON: `{"tables": [..], "skipped": {name: why}}`, each
    /// table its schema's short form ([`Schema::to_short_json`]) with its
    /// `source`, whether it `exists`, the `columns` (column: field) and
    /// `notes`. [`schemas_from_json`] reads it back.
    pub fn to_json(&self) -> Json {
        let tables: Vec<Json> = self
            .tables
            .iter()
            .map(|t| {
                let mut o = Map::new();
                o.insert("source".into(), json!(t.source));
                if let Json::Object(s) = t.schema.to_short_json() {
                    o.extend(s);
                }
                o.insert("exists".into(), json!(t.exists));
                let columns: Map<String, Json> = t.columns.iter().map(|(c, f)| (c.clone(), json!(t.schema.fields[*f].name))).collect();
                o.insert("columns".into(), Json::Object(columns));
                if !t.notes.is_empty() {
                    o.insert("notes".into(), json!(t.notes));
                }
                Json::Object(o)
            })
            .collect();
        let skipped: Map<String, Json> = self.skipped.iter().map(|(n, why)| (n.clone(), json!(why))).collect();
        json!({"tables": tables, "skipped": skipped})
    }
}

/// What an import did.
#[derive(Clone, Debug, Default)]
pub struct Imported {
    pub tables: Vec<TableImport>,
    /// The SQLite tables and views left out, each with the reason.
    pub skipped: Vec<(String, String)>,
}

/// What an import did with one SQLite table.
#[derive(Clone, Debug)]
pub struct TableImport {
    pub source: String,
    /// The Strata table.
    pub table: String,
    pub rows: u64,
    /// Whether the import created the table.
    pub created: bool,
    /// Whether the rows were bulk-loaded (the table was empty) rather than
    /// upserted.
    pub bulk: bool,
    /// The plan's notes ([`TablePlan::notes`]).
    pub notes: Vec<String>,
}

impl Imported {
    /// `{"tables": [{source, table, rows, created, bulk, notes}], "skipped": {name: why}}`.
    pub fn to_json(&self) -> Json {
        let tables: Vec<Json> =
            self.tables.iter().map(|t| json!({"source": t.source, "table": t.table, "rows": t.rows, "created": t.created, "bulk": t.bulk, "notes": t.notes})).collect();
        let skipped: Map<String, Json> = self.skipped.iter().map(|(n, why)| (n.clone(), json!(why))).collect();
        json!({"tables": tables, "skipped": skipped})
    }
}

/// What [`Source::plan`] makes of the SQLite file `sqlite`, for the
/// database `db` (`None`: a new one).
pub fn plan(sqlite: &Path, db: Option<&Db>, opts: &ImportOptions) -> Result<Plan> {
    Source::open(sqlite)?.plan(db, opts)
}

/// Imports the SQLite file `sqlite` into `db` ([`Source::plan`], then
/// [`Source::load`]).
pub fn import(db: &Db, sqlite: &Path, opts: &ImportOptions) -> Result<Imported> {
    let src = Source::open(sqlite)?;
    let plan = src.plan(Some(db), opts)?;
    src.load(db, &plan)
}

/// Creates a database at `path` from the SQLite file `sqlite`: its first
/// table is the first table of the plan ([`Source::plan`]), and
/// [`Source::load`] creates the others.
pub fn import_new(sqlite: &Path, path: &Path, db_opts: DbOptions, opts: &ImportOptions) -> Result<(Db, Imported)> {
    let src = Source::open(sqlite)?;
    let plan = src.plan(None, opts)?;
    let first = plan.tables.first().ok_or_else(|| Error::Schema(format!("{}: no table to import", sqlite.display())))?;
    let db = Db::create(path, first.schema.clone(), db_opts)?;
    let r = src.load(&db, &plan)?;
    Ok((db, r))
}

/// A SQLite file, open for reading.
pub struct Source {
    conn: Connection,
}

impl Source {
    pub fn open(path: &Path) -> Result<Source> {
        if !path.is_file() {
            return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{}: no such file", path.display()))));
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        // The first read of a file that is not a SQLite database fails.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).map_err(|e| Error::Schema(format!("{}: not a SQLite database ({e})", path.display())))?;
        Ok(Source { conn })
    }

    /// The tables an import makes, with their schemas. A SQLite table goes
    /// into the Strata table of its name (in any case; spelled as a Strata
    /// identifier, letters, digits and underscores): into `db`'s table of
    /// that name when it has one, else into a new table with the schema
    /// given for it ([`ImportOptions::schemas`]), the schema an export
    /// kept for it ([`super::META_TABLE`]), or one made from the SQLite
    /// table's declarations and values (see [`super`]).
    pub fn plan(&self, db: Option<&Db>, opts: &ImportOptions) -> Result<Plan> {
        self.plan_over(&db.map(Db::catalog).unwrap_or_default(), opts)
    }

    /// [`Source::plan`] for a database whose tables are `existing`, such as
    /// a server's, which may hold them on shards.
    pub fn plan_over(&self, existing: &[Arc<Schema>], opts: &ImportOptions) -> Result<Plan> {
        let cat = catalog(&self.conn)?;
        let mut skipped = cat.skipped.clone();
        let mut tables: Vec<&Table> = Vec::new();
        if opts.tables.is_empty() {
            tables.extend(cat.tables.iter());
        } else {
            for name in &opts.tables {
                let t = cat.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name)).ok_or_else(|| match skipped.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                    Some((n, why)) => Error::Schema(format!("table {n} cannot be imported: {why}")),
                    None => Error::Schema(format!("the SQLite file has no table {name}")),
                })?;
                if !tables.iter().any(|x| std::ptr::eq(*x, t)) {
                    tables.push(t);
                }
            }
            skipped.retain(|(n, _)| opts.tables.iter().any(|t| t.eq_ignore_ascii_case(n)));
        }
        // Strata's names for the tables, distinct in any case.
        let mut used = HashSet::new();
        let names: Vec<String> = tables.iter().map(|t| distinct(ident(&t.name), &mut used)).collect();
        let given = |t: &Table, name: &str| {
            let by_source = opts.schemas.iter().find(|g| g.source.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&t.name)));
            by_source.or_else(|| opts.schemas.iter().find(|g| g.source.is_none() && (g.schema.table.eq_ignore_ascii_case(name) || g.schema.table.eq_ignore_ascii_case(&t.name))))
        };
        let mut plans = Vec::new();
        // The tables made from SQLite declarations, by position in `plans`,
        // with their single-column foreign keys to resolve.
        let mut inferred = Vec::new();
        for (t, name) in tables.iter().zip(&names) {
            let stats = scan(&self.conn, t)?;
            let mut p = if let Some(s) = existing.iter().find(|s| s.table.eq_ignore_ascii_case(name)) {
                into_schema(t, &stats, s, true, false, opts)
            } else if let Some(g) = given(t, name) {
                into_schema(t, &stats, &g.schema, false, false, opts)
            } else if let Some(s) = cat.meta.get(&t.name.to_ascii_lowercase()) {
                into_schema(t, &stats, s, false, true, opts)
            } else {
                inferred.push(plans.len());
                infer(t, name, &stats, &cat, &tables, opts)?
            };
            if p.schema.table != t.name {
                p.notes.insert(0, format!("table {:?} is {}", t.name, p.schema.table));
            }
            plans.push(p);
        }
        resolve_references(&mut plans, &inferred, &tables)?;
        let plans = order(plans);
        Ok(Plan { tables: plans, skipped })
    }

    /// A plan for the SQLite table `source` into a table with `schema`,
    /// which exists: the columns go into the fields of their names.
    pub fn plan_into(&self, source: &str, schema: &Schema, opts: &ImportOptions) -> Result<TablePlan> {
        let cat = catalog(&self.conn)?;
        let t = cat.tables.iter().find(|t| t.name.eq_ignore_ascii_case(source)).ok_or_else(|| Error::Schema(format!("the SQLite file has no table {source}")))?;
        let stats = scan(&self.conn, t)?;
        Ok(into_schema(t, &stats, schema, true, false, opts))
    }

    /// The SQLite tables the file has, by name, and the views and tables
    /// an import leaves out with the reason.
    pub fn tables(&self) -> Result<(Vec<String>, Vec<(String, String)>)> {
        let cat = catalog(&self.conn)?;
        Ok((cat.tables.into_iter().map(|t| t.name).collect(), cat.skipped))
    }

    /// Creates the tables of `plan` that `db` lacks and writes their rows:
    /// into an empty table in one bulk load each (all or nothing), into a
    /// table with rows upserted by key ([`IMPORT_BATCH`] rows a
    /// transaction). Embedded vectors the rows leave out are computed when
    /// `db` has the field's model ([`Db::open_embedders`]).
    pub fn load(&self, db: &Db, plan: &Plan) -> Result<Imported> {
        let mut out = Imported { tables: Vec::new(), skipped: plan.skipped.clone() };
        for t in &plan.tables {
            let name = t.schema.table.clone();
            if db.storage().table(&name).is_none() {
                db.create_table(t.schema.clone())?;
            }
            let empty = db.snapshot().table(&name).is_none_or(|s| s.n_live_docs() == 0);
            let rows = if empty {
                self.documents(t, |docs| {
                    let mut n = 0u64;
                    db.try_load_into(&name, docs.inspect(|d| n += d.is_ok() as u64))?;
                    Ok(n)
                })?
            } else {
                let keyed = db.storage().table(&name).is_some_and(|s| s.primary_key.is_some());
                self.documents(t, |docs| write(db, &name, keyed, docs))?
            };
            out.tables.push(TableImport { source: t.source.clone(), table: name, rows, created: !t.exists, bulk: empty, notes: t.notes.clone() });
        }
        Ok(out)
    }

    /// Calls `f` with the rows of `t.source` as documents of `t.schema`
    /// (fields without a column take their defaults, generated fields are
    /// filled in), or the first error naming the row.
    pub fn documents<R>(&self, t: &TablePlan, f: impl FnOnce(&mut dyn Iterator<Item = Result<Document>>) -> Result<R>) -> Result<R> {
        let cols: Vec<String> = t.columns.iter().map(|(c, _)| quote(c)).collect();
        let sql = format!("SELECT {} FROM {}", if cols.is_empty() { "NULL".to_string() } else { cols.join(", ") }, quote(&t.source));
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let schema = &t.schema;
        let start = if schema.has_defaults() { schema.defaults(crate::timestamp::now())? } else { vec![Value::Null; schema.fields.len()] };
        let mut n = 0u64;
        let mut docs = std::iter::from_fn(|| {
            let row = match rows.next() {
                Ok(Some(r)) => r,
                Ok(None) => return None,
                Err(e) => return Some(Err(e.into())),
            };
            n += 1;
            let mut doc = start.clone();
            for (i, (&(_, f), &read)) in t.columns.iter().zip(&t.reads).enumerate() {
                let v = row.get_ref(i).map_err(Error::from).and_then(|v| value(schema, f, v, read));
                match v {
                    Ok(v) => doc[f] = v,
                    Err(e) => return Some(Err(Error::Schema(format!("table {} row {n}: {}", t.source, message(e))))),
                }
            }
            schema.fill_generated(&mut doc);
            Some(Ok(doc))
        });
        f(&mut docs)
    }
}

/// Writes `docs` into a table with rows, [`IMPORT_BATCH`] a transaction.
fn write(db: &Db, table: &str, keyed: bool, docs: &mut dyn Iterator<Item = Result<Document>>) -> Result<u64> {
    let mut n = 0;
    loop {
        let batch = (&mut *docs).take(IMPORT_BATCH).collect::<Result<Vec<_>>>()?;
        if batch.is_empty() {
            return Ok(n);
        }
        let mut batch = batch;
        db.fill_in(table, &mut batch)?;
        n += batch.len() as u64;
        let mut txn = db.storage().begin()?;
        let mut t = txn.table(table)?;
        for d in batch {
            if keyed {
                t.upsert(d)?;
            } else {
                t.insert(d)?;
            }
        }
        txn.commit()?;
    }
}

fn message(e: Error) -> String {
    match e {
        Error::Schema(m) | Error::Query(m) => m,
        e => e.to_string(),
    }
}

// ---------------------------------------------------------------- catalog

/// What a SQLite file holds.
struct Catalog {
    tables: Vec<Table>,
    skipped: Vec<(String, String)>,
    /// FTS tables over other tables' text (`content=`).
    fts: Vec<Fts>,
    /// Schemas an export kept ([`META_TABLE`]), by table name in lower case.
    meta: HashMap<String, Schema>,
}

struct Table {
    name: String,
    columns: Vec<Column>,
    /// The primary key's columns, in order.
    pk: Vec<String>,
    /// Foreign keys: the columns, the table, and the columns there (empty:
    /// its primary key).
    fks: Vec<(Vec<String>, String, Vec<String>)>,
    /// Columns in some index, in lower case.
    indexed: HashSet<String>,
    /// UNIQUE constraints and indexes other than the primary key.
    unique: Vec<Vec<String>>,
    /// Whether the table has CHECK constraints.
    checks: bool,
    triggers: Vec<String>,
    /// The table is an FTS table holding its own text.
    fts: Option<Fts>,
}

struct Column {
    name: String,
    decl: String,
    not_null: bool,
    default: Option<String>,
    generated: bool,
}

#[derive(Clone)]
struct Fts {
    /// The table whose text it indexes; `None` for its own text, `""` for
    /// none (contentless).
    content: Option<String>,
    /// Its columns, each with whether it is indexed.
    columns: Vec<(String, bool)>,
    analyzer: Analyzer,
}

fn catalog(conn: &Connection) -> Result<Catalog> {
    let mut stmt = conn.prepare("SELECT type, name, coalesce(sql, '') FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY rowid")?;
    let entries = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let is_virtual = |sql: &str| sql.trim_start().get(..20).is_some_and(|s| s.eq_ignore_ascii_case("CREATE VIRTUAL TABLE"));
    let virtuals: Vec<String> = entries.iter().filter(|(_, _, sql)| is_virtual(sql)).map(|(_, n, _)| format!("{}_", n.to_ascii_lowercase())).collect();
    let mut cat = Catalog { tables: Vec::new(), skipped: Vec::new(), fts: Vec::new(), meta: HashMap::new() };
    // External-content FTS indexes, by name, with the table they index.
    let mut indexes: Vec<(String, String)> = Vec::new();
    for (ty, name, sql) in &entries {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("sqlite_") || lower == META_TABLE {
            continue;
        }
        if ty == "view" {
            cat.skipped.push((name.clone(), "a view (Strata has no views)".into()));
            continue;
        }
        if is_virtual(sql) {
            let (module, args) = virtual_args(sql);
            match fts(&module, &args) {
                Some(f) => match f.content.as_deref() {
                    None => {
                        let mut t = read_table(conn, name, sql)?;
                        t.fts = Some(f);
                        cat.tables.push(t);
                    }
                    Some("") => cat.skipped.push((name.clone(), format!("a contentless {module} index, which holds no text"))),
                    Some(c) => {
                        cat.skipped.push((name.clone(), format!("the {module} index of {c} (the columns it indexes are TEXT fields)")));
                        indexes.push((name.to_ascii_lowercase(), c.to_string()));
                        cat.fts.push(f);
                    }
                },
                None => cat.skipped.push((name.clone(), format!("a virtual table (USING {module})"))),
            }
            continue;
        }
        // A virtual table's own tables (an FTS index's data, say).
        if virtuals.iter().any(|v| lower.starts_with(v)) {
            continue;
        }
        cat.tables.push(read_table(conn, name, sql)?);
    }
    // The triggers that keep a table's FTS index in step belong to the
    // index, which Strata keeps itself.
    for t in &mut cat.tables {
        let of: Vec<&str> = indexes.iter().filter(|(_, c)| c.eq_ignore_ascii_case(&t.name)).map(|(n, _)| n.as_str()).collect();
        if of.is_empty() {
            continue;
        }
        let mut keep = Vec::new();
        for trigger in std::mem::take(&mut t.triggers) {
            let sql: String = conn.query_row("SELECT lower(sql) FROM sqlite_master WHERE type = 'trigger' AND name = ?1", [&trigger], |r| r.get(0))?;
            if !of.iter().any(|n| sql.contains(n)) {
                keep.push(trigger);
            }
        }
        t.triggers = keep;
    }
    let has_meta = entries.iter().any(|(ty, n, _)| ty == "table" && n.eq_ignore_ascii_case(META_TABLE));
    if has_meta {
        let mut stmt = conn.prepare(&format!("SELECT name, schema FROM {}", quote(META_TABLE)))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (name, schema) = row?;
            // A schema this version cannot read: the table is made from SQLite's.
            if let Ok(s) = Schema::from_json(&schema) {
                cat.meta.insert(name.to_ascii_lowercase(), s);
            }
        }
    }
    Ok(cat)
}

fn read_table(conn: &Connection, name: &str, sql: &str) -> Result<Table> {
    let mut stmt = conn.prepare("SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY cid")?;
    let rows = stmt.query_map([name], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, i64>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?))
    })?;
    let mut columns = Vec::new();
    let mut pk = Vec::new();
    for row in rows {
        let (cname, decl, not_null, default, pk_at, hidden) = row?;
        // Hidden columns of a virtual table.
        if hidden == 1 {
            continue;
        }
        if pk_at > 0 {
            pk.push((pk_at, cname.clone()));
        }
        columns.push(Column { name: cname, decl, not_null: not_null != 0, default, generated: hidden >= 2 });
    }
    pk.sort();
    let mut fks: Vec<(i64, Vec<String>, String, Vec<String>)> = Vec::new();
    let mut stmt = conn.prepare("SELECT id, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1) ORDER BY id, seq")?;
    let rows = stmt.query_map([name], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))?;
    for row in rows {
        let (id, table, from, to) = row?;
        match fks.last_mut() {
            Some(fk) if fk.0 == id => {
                fk.1.push(from);
                fk.3.extend(to);
            }
            _ => fks.push((id, vec![from], table, to.into_iter().collect())),
        }
    }
    let mut indexed = HashSet::new();
    let mut unique = Vec::new();
    let mut stmt = conn.prepare("SELECT name, \"unique\", origin FROM pragma_index_list(?1)")?;
    let indexes = stmt.query_map([name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (index, is_unique, origin) in indexes {
        let mut stmt = conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
        // A column of an expression index has no name.
        let cols: Vec<String> = stmt.query_map([&index], |r| r.get::<_, Option<String>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().flatten().collect();
        indexed.extend(cols.iter().map(|c| c.to_ascii_lowercase()));
        if is_unique != 0 && origin != "pk" && !cols.is_empty() {
            unique.push(cols);
        }
    }
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name")?;
    let triggers = stmt.query_map([name], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let checks = sql.to_ascii_uppercase().contains("CHECK");
    Ok(Table { name: name.to_string(), columns, pk: pk.into_iter().map(|(_, c)| c).collect(), fks: fks.into_iter().map(|(_, f, t, to)| (f, t, to)).collect(), indexed, unique, checks, triggers, fts: None })
}

/// The module and arguments of `CREATE VIRTUAL TABLE .. USING module(args)`.
fn virtual_args(sql: &str) -> (String, Vec<String>) {
    let upper = sql.to_ascii_uppercase();
    let b = upper.as_bytes();
    let at = upper.match_indices("USING").map(|(i, _)| i).find(|&i| i > 0 && !b[i - 1].is_ascii_alphanumeric() && b.get(i + 5).is_some_and(|c| c.is_ascii_whitespace()));
    let Some(at) = at else { return (String::new(), Vec::new()) };
    let rest = sql[at + 5..].trim_start();
    let (module, args) = match (rest.find('('), rest.rfind(')')) {
        (Some(o), Some(c)) if c > o => (&rest[..o], split_args(&rest[o + 1..c])),
        _ => (rest, Vec::new()),
    };
    (module.trim().to_ascii_lowercase(), args)
}

/// Splits `a, b = 'x, y', c(d, e)` at the commas outside quotes and
/// parentheses.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut quote, mut depth) = (String::new(), None::<char>, 0usize);
    for c in s.chars() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '[') => quote = Some(']'),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                out.push(std::mem::take(&mut cur).trim().to_string());
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// A SQL name or string without its quotes.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    match (b.first(), b.last()) {
        (Some(&q @ (b'\'' | b'"' | b'`')), Some(&e)) if s.len() >= 2 && e == q => {
            let q = q as char;
            s[1..s.len() - 1].replace(&format!("{q}{q}"), &q.to_string())
        }
        (Some(b'['), Some(b']')) => s[1..s.len() - 1].to_string(),
        _ => s.to_string(),
    }
}

/// An FTS3, FTS4 or FTS5 table's columns and options.
fn fts(module: &str, args: &[String]) -> Option<Fts> {
    if !matches!(module, "fts3" | "fts4" | "fts5") {
        return None;
    }
    let mut f = Fts { content: None, columns: Vec::new(), analyzer: Analyzer::Simple };
    let mut not_indexed = Vec::new();
    for a in args {
        // FTS3's old `tokenize porter` has no `=`.
        let old_tokenize = a.get(..9).is_some_and(|p| p.eq_ignore_ascii_case("tokenize "));
        if outside_quotes_eq(a) || old_tokenize {
            let (k, v) = match a.split_once('=') {
                Some((k, v)) if !old_tokenize => (unquote(k).to_ascii_lowercase(), unquote(v)),
                _ => ("tokenize".to_string(), unquote(&a[9..])),
            };
            match k.as_str() {
                "content" => f.content = Some(v),
                "tokenize" if v.to_ascii_lowercase().contains("porter") => f.analyzer = Analyzer::English,
                "notindexed" => not_indexed.push(v.to_ascii_lowercase()),
                _ => {}
            }
            continue;
        }
        let words: Vec<&str> = a.split_whitespace().collect();
        let Some(first) = words.first() else { continue };
        let indexed = !words[1..].iter().any(|w| w.eq_ignore_ascii_case("UNINDEXED"));
        f.columns.push((unquote(first), indexed));
    }
    for (c, indexed) in &mut f.columns {
        if not_indexed.contains(&c.to_ascii_lowercase()) {
            *indexed = false;
        }
    }
    Some(f)
}

/// Whether `a` has an `=` outside quotes.
fn outside_quotes_eq(a: &str) -> bool {
    let mut quote = None::<char>;
    for c in a.chars() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '=') => return true,
            _ => {}
        }
    }
    false
}

// ------------------------------------------------------------------ names

/// `name` as a Strata identifier: letters, digits and underscores, not
/// starting with a digit.
fn ident(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

// ------------------------------------------------------------------ types

/// What a column's declared type says, by SQLite's affinity rules and the
/// type names apps use.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hint {
    Int,
    Real,
    /// `DECIMAL`, `NUMERIC`, `MONEY`: numbers, whole or not.
    Decimal,
    Numeric,
    Text,
    /// `BLOB` or no type.
    Blob,
    Bool,
    Time,
    Json,
    Vector(usize),
}

fn hint(decl: &str) -> Hint {
    let d = decl.trim().to_ascii_uppercase();
    for p in ["F32_BLOB", "FLOAT32", "VECTOR"] {
        if let Some(rest) = d.strip_prefix(p) {
            let n = rest.trim().strip_prefix('(').and_then(|r| r.strip_suffix(')')).and_then(|n| n.trim().parse::<usize>().ok());
            if let Some(n) = n.filter(|&n| n > 0) {
                return Hint::Vector(n);
            }
        }
    }
    if d.starts_with("BOOL") {
        Hint::Bool
    } else if d.contains("INT") {
        Hint::Int
    } else if d.contains("DATE") || d.contains("TIME") {
        Hint::Time
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        Hint::Text
    } else if d == "JSON" {
        Hint::Json
    } else if d.is_empty() || d.contains("BLOB") {
        Hint::Blob
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        Hint::Real
    } else if d.contains("DEC") || d.contains("NUM") || d.contains("MONEY") {
        Hint::Decimal
    } else {
        Hint::Numeric
    }
}

/// What a column holds.
/// The distinct texts a column's [`Stats`] count, at most.
const DISTINCT: usize = 10_000;

fn hash(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

#[derive(Debug)]
struct Stats {
    nulls: u64,
    ints: u64,
    reals: u64,
    texts: u64,
    blobs: u64,
    /// The largest integer, in absolute value.
    max_abs: u64,
    /// An integer or real other than 0 and 1.
    not_bool_number: bool,
    /// The longest text, in bytes, and the words of all of them (runs
    /// without spaces that hold a letter: a phone number has none).
    text_max: usize,
    words: u64,
    /// Hashes of the distinct texts, up to [`DISTINCT`]; `None` past it.
    distinct: Option<HashSet<u64>>,
    /// A text that is not an integer, a number, or (for the hints that ask)
    /// a timestamp, a boolean, a vector of the hinted dimension, or a JSON
    /// array of strings or integers.
    not_int: bool,
    not_float: bool,
    not_time: bool,
    not_bool: bool,
    not_vector: bool,
    not_list: bool,
    /// A JSON array holds a string.
    list_strings: bool,
    /// A blob that is not a vector of the hinted dimension.
    not_vector_blob: bool,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            nulls: 0,
            ints: 0,
            reals: 0,
            texts: 0,
            blobs: 0,
            max_abs: 0,
            not_bool_number: false,
            text_max: 0,
            words: 0,
            distinct: Some(HashSet::new()),
            not_int: false,
            not_float: false,
            not_time: false,
            not_bool: false,
            not_vector: false,
            not_list: false,
            list_strings: false,
            not_vector_blob: false,
        }
    }
}

impl Stats {
    /// Whether the texts are labels rather than prose: short (2.5 words or
    /// fewer on average, none over 256 bytes), or few values repeated (at
    /// least 100 texts, each value ten times on average).
    fn labels(&self) -> bool {
        let short = self.words as f64 <= 2.5 * self.texts as f64;
        let repeated = self.texts >= 100 && self.distinct.as_ref().is_some_and(|d| d.len() as u64 * 10 <= self.texts);
        self.text_max <= 256 && (short || repeated)
    }

    fn add(&mut self, v: ValueRef<'_>, hint: Hint) {
        match v {
            ValueRef::Null => self.nulls += 1,
            ValueRef::Integer(i) => {
                self.ints += 1;
                self.max_abs = self.max_abs.max(i.unsigned_abs());
                self.not_bool_number |= !(0..=1).contains(&i);
            }
            ValueRef::Real(r) => {
                self.reals += 1;
                self.not_bool_number |= r != 0.0 && r != 1.0;
            }
            ValueRef::Text(b) => {
                self.texts += 1;
                let s = String::from_utf8_lossy(b);
                let t = s.trim();
                self.text_max = self.text_max.max(b.len());
                self.words += s.split_whitespace().filter(|w| w.chars().any(char::is_alphabetic)).count() as u64;
                if let Some(d) = &mut self.distinct {
                    d.insert(hash(b));
                    if d.len() > DISTINCT {
                        self.distinct = None;
                    }
                }
                self.not_int |= t.parse::<i64>().is_err();
                self.not_float |= !t.parse::<f64>().is_ok_and(f64::is_finite);
                match hint {
                    Hint::Time => self.not_time |= crate::timestamp::parse(t).is_none(),
                    Hint::Bool => self.not_bool |= !matches!(t.to_ascii_lowercase().as_str(), "true" | "false" | "t" | "f" | "1" | "0"),
                    Hint::Vector(dim) => self.not_vector |= !serde_json::from_str::<Vec<f32>>(t).is_ok_and(|v| v.len() == dim),
                    Hint::Json => match serde_json::from_str::<Json>(t) {
                        Ok(Json::Array(xs)) => {
                            for x in xs {
                                match x {
                                    Json::String(_) => self.list_strings = true,
                                    Json::Number(n) if n.is_i64() => {}
                                    _ => self.not_list = true,
                                }
                            }
                        }
                        _ => self.not_list = true,
                    },
                    _ => {}
                }
            }
            ValueRef::Blob(b) => {
                self.blobs += 1;
                self.not_vector_blob |= !matches!(hint, Hint::Vector(dim) if b.len() == 4 * dim);
            }
        }
    }

    /// The microseconds in a unit of the Unix time the integers hold:
    /// seconds up to 10^11 (the year 5138), else milliseconds up to 10^14,
    /// else microseconds.
    fn time_unit(&self) -> i64 {
        match self.max_abs {
            0..100_000_000_000 => 1_000_000,
            100_000_000_000..100_000_000_000_000 => 1_000,
            _ => 1,
        }
    }

    fn numbers(&self) -> u64 {
        self.ints + self.reals
    }
}

/// Reads every row of `t` once, gathering each column's [`Stats`].
fn scan(conn: &Connection, t: &Table) -> Result<Vec<Stats>> {
    let hints: Vec<Hint> = t.columns.iter().map(|c| hint(&c.decl)).collect();
    let mut stats: Vec<Stats> = t.columns.iter().map(|_| Stats::default()).collect();
    if t.columns.is_empty() {
        return Ok(stats);
    }
    let cols: Vec<String> = t.columns.iter().map(|c| quote(&c.name)).collect();
    let mut stmt = conn.prepare(&format!("SELECT {} FROM {}", cols.join(", "), quote(&t.name)))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for (i, s) in stats.iter_mut().enumerate() {
            s.add(row.get_ref(i)?, hints[i]);
        }
    }
    Ok(stats)
}

/// What a column of a table being made becomes.
enum Choice {
    Field(Field, Read),
    Skip(String),
}

/// How a column is used, beyond its type.
#[derive(Default)]
struct Role {
    key: bool,
    reference: bool,
    indexed: bool,
    /// Indexed by an FTS table, with this analyzer.
    fts: Option<Analyzer>,
}

fn choose(name: &str, hint: Hint, st: &Stats, role: &Role, opts: &ImportOptions) -> Choice {
    let field = |ty: FieldType| Choice::Field(Field::new(name, ty), SECONDS);
    let vector = |dim: usize| field(FieldType::Vector { dim, index: DEFAULT_HNSW });
    if st.blobs > 0 {
        if let Hint::Vector(dim) = hint {
            if !st.not_vector_blob && st.numbers() == 0 && !st.not_vector {
                return vector(dim);
            }
        }
        if opts.blobs_base64 {
            return Choice::Field(Field::new(name, FieldType::Keyword).stored_only(), Read::Base64);
        }
        return Choice::Skip("BLOB values: Strata has no binary type (blobs_base64 keeps them as base64 text)".into());
    }
    match hint {
        Hint::Vector(dim) if st.numbers() == 0 && !st.not_vector => return vector(dim),
        Hint::Bool if !st.not_bool_number && !st.not_bool => return Choice::Field(Field::boolean(name), SECONDS),
        Hint::Time if !st.not_time && !role.key => return Choice::Field(Field::timestamp(name), Read::Plain { time_unit: st.time_unit() }),
        Hint::Json if st.texts > 0 && st.numbers() == 0 && !st.not_list => {
            let ty = if st.list_strings { FieldType::Keyword } else { FieldType::Int };
            return Choice::Field(Field::new(name, ty).multi(), SECONDS);
        }
        _ => {}
    }
    if st.texts == 0 {
        if st.reals > 0 || matches!(hint, Hint::Real | Hint::Decimal) && !role.key && !role.reference {
            return field(FieldType::Float);
        }
        if st.ints > 0 || hint == Hint::Int {
            return field(FieldType::Int);
        }
    } else if hint == Hint::Int && !st.not_int && st.reals == 0 {
        return field(FieldType::Int);
    } else if matches!(hint, Hint::Real | Hint::Decimal) && !st.not_float && !role.key {
        return field(FieldType::Float);
    }
    // Text, with any numbers as text.
    let ty = match role.fts {
        _ if role.key || role.reference => FieldType::Keyword,
        Some(a) => {
            let mut f = Field::new(name, FieldType::Text { positions: true });
            f.analyzer = Some(a);
            return Choice::Field(f, SECONDS);
        }
        None if role.indexed || st.labels() => FieldType::Keyword,
        None => FieldType::Text { positions: true },
    };
    field(ty)
}

/// The JSON of a constant SQL `DEFAULT` for `f`, if it is one `f` takes.
fn default_of(sql: &str, f: &Field) -> Option<Json> {
    let mut s = sql.trim();
    while s.starts_with('(') && s.ends_with(')') {
        s = s[1..s.len() - 1].trim();
    }
    let upper = s.to_ascii_uppercase().replace(' ', "");
    let text = matches!(f.ty, FieldType::Text { .. } | FieldType::Keyword);
    let j = if upper == "NULL" {
        return None;
    } else if matches!(upper.as_str(), "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "DATETIME('NOW')" | "DATETIME(\"NOW\")") {
        if f.logical != Some(Logical::Timestamp) {
            return None;
        }
        json!("now")
    } else if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        json!(s[1..s.len() - 1].replace("''", "'"))
    } else if upper == "TRUE" || upper == "FALSE" {
        json!(upper == "TRUE")
    } else if let Ok(i) = s.parse::<i64>() {
        if text { json!(s) } else { json!(i) }
    } else if let Ok(x) = s.parse::<f64>().map(|x| x.is_finite().then_some(x)) {
        if text { json!(s) } else { json!(x?) }
    } else {
        return None;
    };
    let mut f = f.clone();
    f.default = Some(j.clone());
    f.default_value(0).is_ok().then_some(j)
}

/// A table made from SQLite's declarations and values (references are
/// added by [`resolve_references`]).
fn infer(t: &Table, name: &str, stats: &[Stats], cat: &Catalog, tables: &[&Table], opts: &ImportOptions) -> Result<TablePlan> {
    let mut notes = Vec::new();
    let lower = |s: &str| s.to_ascii_lowercase();
    // Columns an FTS table indexes.
    let mut fts: HashMap<String, Analyzer> = HashMap::new();
    let indexes_of = cat.fts.iter().filter(|f| f.content.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(&t.name))).chain(t.fts.iter());
    for f in indexes_of {
        for (c, indexed) in &f.columns {
            if *indexed {
                fts.insert(lower(c), f.analyzer);
            }
        }
    }
    let key = (t.pk.len() == 1).then(|| lower(&t.pk[0]));
    let referencing: HashSet<String> = t.fks.iter().filter(|(from, target, _)| from.len() == 1 && tables.iter().any(|x| x.name.eq_ignore_ascii_case(target))).map(|(from, _, _)| lower(&from[0])).collect();
    let mut used = HashSet::new();
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    let mut reads = Vec::new();
    for (c, st) in t.columns.iter().zip(stats) {
        let fname = distinct(ident(&c.name), &mut used);
        let role = Role { key: key.as_deref() == Some(&lower(&c.name)), reference: referencing.contains(&lower(&c.name)), indexed: t.indexed.contains(&lower(&c.name)), fts: fts.get(&lower(&c.name)).copied() };
        let h = hint(&c.decl);
        let (mut f, read) = match choose(&fname, h, st, &role, opts) {
            Choice::Field(f, read) => (f, read),
            Choice::Skip(why) => {
                notes.push(format!("column {:?} left out: {why}", c.name));
                continue;
            }
        };
        if fname != c.name {
            notes.push(format!("column {:?} is field {fname}", c.name));
        }
        if read == Read::Base64 {
            notes.push(format!("column {:?}: BLOB values as base64 text", c.name));
        }
        if st.numbers() > 0 && matches!(f.ty, FieldType::Text { .. } | FieldType::Keyword) && !f.multi {
            let declared = if c.decl.is_empty() { String::new() } else { format!(" ({})", c.decl) };
            if st.texts > 0 && !matches!(h, Hint::Text | Hint::Blob | Hint::Numeric) {
                notes.push(format!("column {:?}{declared} holds text, so its numbers are text too", c.name));
            }
        }
        if f.logical == Some(Logical::Timestamp) && st.numbers() > 0 {
            let unit = match read {
                Read::Plain { time_unit: 1_000_000 } => "seconds",
                Read::Plain { time_unit: 1_000 } => "milliseconds",
                _ => "microseconds",
            };
            notes.push(format!("column {:?}: integers read as Unix time in {unit}, reals under 10^7 as Julian days", c.name));
        }
        if c.generated {
            notes.push(format!("column {:?} is generated in SQLite: its values are imported", c.name));
        }
        if c.not_null && !role.key && st.nulls == 0 {
            f.not_null = true;
        }
        if let Some(d) = &c.default {
            match (!role.key && !f.multi && !matches!(f.ty, FieldType::Vector { .. })).then(|| default_of(d, &f)).flatten() {
                Some(j) => f.default = Some(j),
                None if d.trim().eq_ignore_ascii_case("NULL") => {}
                None => notes.push(format!("column {:?}: DEFAULT {d} left out", c.name)),
            }
        }
        columns.push((c.name.clone(), fields.len()));
        reads.push(read);
        fields.push(f);
    }
    // The key: one INT or KEYWORD column without NULLs.
    let mut primary_key = None;
    match t.pk.len() {
        0 => {}
        1 => {
            let c = &t.pk[0];
            let at = t.columns.iter().position(|x| x.name == *c).expect("a column of the table");
            match columns.iter().find(|(x, _)| x == c).map(|&(_, f)| f) {
                Some(_) if stats[at].nulls > 0 => notes.push(format!("primary key {c:?} holds NULLs, so the table has no key")),
                Some(f) if matches!(fields[f].ty, FieldType::Int | FieldType::Keyword) && fields[f].logical.is_none() && !fields[f].multi => primary_key = Some(fields[f].name.clone()),
                Some(f) => notes.push(format!("primary key {c:?} is {}: a key is INT or KEYWORD, so the table has none", fields[f].type_name())),
                None => notes.push(format!("primary key {c:?} is left out, so the table has no key")),
            }
        }
        _ => notes.push(format!("composite primary key ({}): a key is one field, so the table has none", t.pk.join(", "))),
    }
    for (from, target, _) in &t.fks {
        if from.len() > 1 {
            notes.push(format!("foreign key ({}) to {target}: a reference is one field, so it is left out", from.join(", ")));
        } else if !tables.iter().any(|x| x.name.eq_ignore_ascii_case(target)) {
            notes.push(format!("foreign key {:?} to {target}: that table is not imported", from[0]));
        }
    }
    for u in &t.unique {
        notes.push(format!("UNIQUE ({}) is not enforced", u.join(", ")));
    }
    if t.checks {
        notes.push("CHECK constraints are not imported".into());
    }
    if !t.triggers.is_empty() {
        notes.push(format!("triggers not imported: {}", t.triggers.join(", ")));
    }
    let schema = Schema::new(name, fields, primary_key.as_deref()).map_err(|e| Error::Schema(format!("table {}: {}", t.name, message(e))))?;
    Ok(TablePlan { source: t.name.clone(), schema, exists: false, columns, notes, reads })
}

/// A plan of `t` into a table with `schema`: each column fills the field
/// of its name (as given or as Strata spells it). `meta`: the schema is the
/// one an export kept, whose generated fields need no note.
fn into_schema(t: &Table, stats: &[Stats], schema: &Schema, exists: bool, meta: bool, opts: &ImportOptions) -> TablePlan {
    let mut notes = Vec::new();
    let mut columns = Vec::new();
    let mut reads = Vec::new();
    for (c, st) in t.columns.iter().zip(stats) {
        let Some(f) = schema.field_index(&c.name).or_else(|| schema.field_index(&ident(&c.name))) else {
            notes.push(format!("column {:?} left out: {} has no field of its name", c.name, schema.table));
            continue;
        };
        let field = &schema.fields[f];
        if !field.generated_from.is_empty() {
            if !meta {
                notes.push(format!("column {:?} left out: field {} is generated from {}", c.name, field.name, field.generated_from.join(", ")));
            }
            continue;
        }
        if columns.iter().any(|&(_, g)| g == f) {
            notes.push(format!("column {:?} left out: another column fills field {}", c.name, field.name));
            continue;
        }
        let read = if st.blobs > 0 && opts.blobs_base64 && matches!(field.ty, FieldType::Keyword | FieldType::Text { .. }) && field.media.is_none() { Read::Base64 } else { Read::Plain { time_unit: st.time_unit() } };
        columns.push((c.name.clone(), f));
        reads.push(read);
    }
    TablePlan { source: t.name.clone(), schema: schema.clone(), exists, columns, notes, reads }
}

/// Adds references for the single-column foreign keys of the tables made
/// from SQLite declarations (`inferred`, by position in `plans`) to an
/// imported table's key, when the types allow (a key's integers as text
/// for a KEYWORD key).
fn resolve_references(plans: &mut [TablePlan], inferred: &[usize], tables: &[&Table]) -> Result<()> {
    for &i in inferred {
        let t = tables.iter().find(|t| t.name == plans[i].source).expect("a planned table");
        for (from, target, to) in &t.fks {
            if from.len() != 1 {
                continue;
            }
            let Some(j) = plans.iter().position(|p| p.source.eq_ignore_ascii_case(target)) else { continue };
            let Some(&(_, f)) = plans[i].columns.iter().find(|(c, _)| *c == from[0]) else { continue };
            let note = |p: &mut TablePlan, why: String| p.notes.push(format!("foreign key {:?} to {target}: {why}", from[0]));
            let target_schema = &plans[j].schema;
            let Some(k) = target_schema.primary_key else {
                let why = format!("{} has no key, so it is not a reference", target_schema.table);
                note(&mut plans[i], why);
                continue;
            };
            let key = &target_schema.fields[k];
            let to_key = to.is_empty() || plans[j].columns.iter().any(|(c, g)| *g == k && c.eq_ignore_ascii_case(&to[0]));
            if !to_key {
                let why = format!("{} is not the key of {}, so it is not a reference", to[0], target_schema.table);
                note(&mut plans[i], why);
                continue;
            }
            let (key_ty, table) = (key.ty.clone(), target_schema.table.clone());
            let field = &mut plans[i].schema.fields[f];
            if field.ty == FieldType::Int && key_ty == FieldType::Keyword && field.logical.is_none() && !field.multi {
                // Integer codes referencing a text key: as text, to match it.
                field.ty = FieldType::Keyword;
                if let Some(Json::Number(n)) = &field.default {
                    field.default = Some(Json::String(n.to_string()));
                }
            }
            if field.ty != key_ty || field.multi || field.logical.is_some() {
                let why = format!("{} is {} and the key of {table} is {}, so it is not a reference", field.name, field.type_name(), crate::schema::type_name(&key_ty));
                note(&mut plans[i], why);
                continue;
            }
            field.references = Some(table);
        }
        let s = &plans[i].schema;
        plans[i].schema = Schema::new(&s.table, s.fields.clone(), s.primary_key.map(|k| s.fields[k].name.as_str())).map_err(|e| Error::Schema(format!("table {}: {}", plans[i].source, message(e))))?;
    }
    Ok(())
}

/// `plans` with each table after the tables it references, in their
/// order otherwise; a reference that closes a cycle is dropped (with a
/// note), since a table can only reference one created before it.
fn order(mut plans: Vec<TablePlan>) -> Vec<TablePlan> {
    let mut out: Vec<TablePlan> = Vec::new();
    while !plans.is_empty() {
        let placed = |r: &str, me: &str| r.eq_ignore_ascii_case(me) || out.iter().any(|p| p.schema.table.eq_ignore_ascii_case(r));
        let pending = |p: &TablePlan, all: &[TablePlan]| {
            p.schema.fields.iter().filter_map(|f| f.references.as_deref()).any(|r| !placed(r, &p.schema.table) && all.iter().any(|q| q.schema.table.eq_ignore_ascii_case(r)))
        };
        match (0..plans.len()).find(|&i| !pending(&plans[i], &plans)) {
            Some(i) => out.push(plans.remove(i)),
            None => {
                // A cycle: the first table drops its references to tables not yet placed.
                let mut p = plans.remove(0);
                let rest: Vec<String> = plans.iter().map(|q| q.schema.table.to_ascii_lowercase()).collect();
                for f in p.schema.fields.iter_mut() {
                    if f.references.as_ref().is_some_and(|r| rest.contains(&r.to_ascii_lowercase())) {
                        p.notes.push(format!("{}: the reference to {} closes a cycle of references, so it is left out", f.name, f.references.take().unwrap_or_default()));
                    }
                }
                out.push(p);
            }
        }
    }
    out
}

// ----------------------------------------------------------------- values

/// The value of a SQLite column for field `f` of `schema`.
fn value(schema: &Schema, f: usize, v: ValueRef<'_>, read: Read) -> Result<Value> {
    let field = &schema.fields[f];
    let key = schema.primary_key == Some(f);
    let bad = |what: String| Error::Schema(format!("bad value for {}: {what}", field.name));
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if let (Read::Base64, ValueRef::Blob(b)) = (read, v) {
        return Ok(Value::Str(base64(b)));
    }
    if let (Some(Logical::Timestamp), Read::Plain { time_unit }) = (field.logical, read) {
        match v {
            ValueRef::Integer(i) => return i.checked_mul(time_unit).map(Value::Int).ok_or_else(|| bad(format!("{i} is out of range"))),
            ValueRef::Real(r) if r.abs() < 1e7 => return Ok(Value::Int(((r - 2_440_587.5) * 86_400_000_000.0).round() as i64)),
            ValueRef::Real(r) if (r * time_unit as f64).abs() < 9.2e18 => return Ok(Value::Int((r * time_unit as f64).round() as i64)),
            ValueRef::Real(r) => return Err(bad(format!("{r} is out of range"))),
            _ => {}
        }
    }
    if field.multi {
        let j = match v {
            ValueRef::Null => return Ok(Value::Null),
            ValueRef::Text(b) => {
                let s = text(b);
                match serde_json::from_str::<Json>(s.trim()) {
                    Ok(j @ Json::Array(_)) => j,
                    _ => Json::String(s),
                }
            }
            ValueRef::Integer(i) => json!(i),
            ValueRef::Real(r) => json!(r),
            ValueRef::Blob(_) => return Err(bad("a BLOB".into())),
        };
        return ingest::field_value(field, key, &j);
    }
    Ok(match (&field.ty, v) {
        (_, ValueRef::Null) => Value::Null,
        (FieldType::Vector { dim, .. }, ValueRef::Blob(b)) => {
            if b.len() != 4 * dim {
                return Err(bad(format!("a BLOB of {} bytes is not {dim} float32 values", b.len())));
            }
            Value::Vector(b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect::<Vec<f32>>().into())
        }
        (_, ValueRef::Blob(b)) => return Err(bad(format!("a BLOB of {} bytes", b.len()))),
        (FieldType::Text { .. } | FieldType::Keyword, ValueRef::Integer(i)) if field.logical.is_none() && field.media.is_none() => Value::Str(i.to_string()),
        (FieldType::Text { .. } | FieldType::Keyword, ValueRef::Real(r)) if field.logical.is_none() && field.media.is_none() => Value::Str(r.to_string()),
        (FieldType::Int, ValueRef::Real(r)) if field.logical.is_none() => {
            if r.fract() != 0.0 || r.abs() >= 9.2e18 {
                return Err(bad(format!("{r} is not an integer")));
            }
            Value::Int(r as i64)
        }
        (FieldType::Int, ValueRef::Text(b)) if field.logical.is_none() => {
            let s = text(b);
            Value::Int(s.trim().parse().map_err(|_| bad(format!("{s:?} is not an integer")))?)
        }
        (FieldType::Float, ValueRef::Real(r)) if !r.is_finite() => return Err(bad(format!("{r} is not a finite number"))),
        (FieldType::Float, ValueRef::Text(b)) => {
            let s = text(b);
            Value::Float(s.trim().parse::<f64>().ok().filter(|x| x.is_finite()).ok_or_else(|| bad(format!("{s:?} is not a number")))?)
        }
        (FieldType::Vector { .. }, ValueRef::Text(b)) => {
            let j: Json = serde_json::from_str(&text(b)).map_err(|e| bad(format!("not a JSON array of numbers ({e})")))?;
            ingest::field_value(field, key, &j)?
        }
        (_, ValueRef::Text(b)) => ingest::field_value(field, key, &Json::String(text(b)))?,
        (_, ValueRef::Integer(i)) => ingest::field_value(field, key, &json!(i))?,
        (_, ValueRef::Real(r)) => ingest::field_value(field, key, &json!(r))?,
    })
}

/// Standard base64, with padding.
fn base64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints() {
        assert_eq!(hint("INTEGER"), Hint::Int);
        assert_eq!(hint("bigint"), Hint::Int);
        assert_eq!(hint("VARCHAR(20)"), Hint::Text);
        assert_eq!(hint("DATETIME"), Hint::Time);
        assert_eq!(hint("timestamp"), Hint::Time);
        assert_eq!(hint("BOOLEAN"), Hint::Bool);
        assert_eq!(hint("DECIMAL(10,2)"), Hint::Decimal);
        assert_eq!(hint("double precision"), Hint::Real);
        assert_eq!(hint(""), Hint::Blob);
        assert_eq!(hint("F32_BLOB(384)"), Hint::Vector(384));
        assert_eq!(hint("vector( 3 )"), Hint::Vector(3));
        assert_eq!(hint("BLOB"), Hint::Blob);
        assert_eq!(hint("JSON"), Hint::Json);
        assert_eq!(hint("UUID"), Hint::Numeric);
    }

    #[test]
    fn fts_args() {
        let (m, a) = virtual_args("CREATE VIRTUAL TABLE \"docs fts\" USING fts5(title, body UNINDEXED, content='docs', content_rowid='id', tokenize = 'porter unicode61')");
        assert_eq!(m, "fts5");
        let f = fts(&m, &a).unwrap();
        assert_eq!(f.content.as_deref(), Some("docs"));
        assert_eq!(f.columns, vec![("title".to_string(), true), ("body".to_string(), false)]);
        assert_eq!(f.analyzer, Analyzer::English);
        let (m, a) = virtual_args("create virtual table t using fts4(a, b, notindexed=b, tokenize=simple)");
        let f = fts(&m, &a).unwrap();
        assert_eq!(f.content, None);
        assert_eq!(f.columns, vec![("a".to_string(), true), ("b".to_string(), false)]);
        assert_eq!(f.analyzer, Analyzer::Simple);
        let (m, a) = virtual_args("CREATE VIRTUAL TABLE r USING rtree(id, minx, maxx)");
        assert_eq!((m.as_str(), a.len()), ("rtree", 3));
        assert!(fts(&m, &a).is_none());
    }

    #[test]
    fn names() {
        assert_eq!(ident("Order Details"), "Order_Details");
        assert_eq!(ident("2024 sales"), "_2024_sales");
        assert_eq!(ident("née"), "n_e");
        let mut used = HashSet::new();
        assert_eq!(distinct("a".into(), &mut used), "a");
        assert_eq!(distinct("A".into(), &mut used), "A_2");
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (raw, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(raw.as_bytes()), enc);
        }
    }

    #[test]
    fn defaults() {
        let int = Field::new("n", FieldType::Int);
        assert_eq!(default_of("0", &int), Some(json!(0)));
        assert_eq!(default_of("(-5)", &int), Some(json!(-5)));
        assert_eq!(default_of("'x'", &int), None);
        let kw = Field::new("s", FieldType::Keyword);
        assert_eq!(default_of("'it''s'", &kw), Some(json!("it's")));
        assert_eq!(default_of("7", &kw), Some(json!("7")));
        let ts = Field::timestamp("t");
        assert_eq!(default_of("CURRENT_TIMESTAMP", &ts), Some(json!("now")));
        assert_eq!(default_of("(datetime('now'))", &ts), Some(json!("now")));
        assert_eq!(default_of("'2026-01-01 00:00:00'", &ts), Some(json!("2026-01-01 00:00:00")));
        assert_eq!(default_of("random()", &int), None);
        let b = Field::boolean("b");
        assert_eq!(default_of("TRUE", &b), Some(json!(true)));
        assert_eq!(default_of("0", &b), Some(json!(0)));
    }
}
