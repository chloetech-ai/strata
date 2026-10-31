//! SQLite databases into Strata, and Strata databases out to SQLite
//! (feature `sqlite`).
//!
//! [`import`] reads the tables of a SQLite file into Strata tables of the
//! same names (spelled as Strata identifiers: `Order Details` becomes
//! `Order_Details`). A table Strata lacks is created with a schema made
//! from the SQLite one and the values it holds; a table it has keeps its
//! schema, and takes the columns whose names match fields. [`plan`] says
//! what an import would do without doing it, and its schemas can be edited
//! and given back ([`ImportOptions::schemas`]).
//!
//! | SQLite column | Strata field |
//! |---|---|
//! | `INTEGER` affinity (any type naming `INT`) | `INT` |
//! | `REAL`, `FLOAT`, `DOUBLE`, `DECIMAL`, `NUMERIC` | `FLOAT` |
//! | `BOOL`, `BOOLEAN` holding 0, 1, true or false | `BOOL` |
//! | `DATE`, `DATETIME`, `TIMESTAMP` holding ISO 8601 text, Unix time or Julian days | `TIMESTAMP` |
//! | text in a column indexed by an FTS3/4/5 table | `TEXT` (`english` analyzer for a `porter` tokenizer, else `simple`) |
//! | other text: a primary key, a foreign key, an indexed column, or labels rather than prose (none over 256 bytes, and 2.5 words or fewer on average, or each value repeated ten times on average) | `KEYWORD` |
//! | other text | `TEXT` |
//! | `F32_BLOB(n)`, `FLOAT32(n)`, `VECTOR(n)` holding float32 blobs or JSON arrays | `VECTOR(n)` |
//! | `JSON` holding arrays of strings or integers | multi-valued `KEYWORD` or `INT` |
//! | no type | from the values: `INT`, `FLOAT`, or text as above |
//! | other `BLOB` | skipped (Strata has no binary type), or base64 text with [`ImportOptions::blobs_base64`] |
//!
//! A name that is already an identifier keeps it, and a respelled one
//! that meets another takes a suffix (`x-y` beside `x_y` is `x_y_2`). A
//! column whose values do not fit its declared type (text in an
//! `INTEGER` column, as SQLite allows) is read as text. In a `FLOAT`
//! column an infinite real is NULL, and integers past 2^53 round; the
//! plan's notes say so. A single-column
//! `INTEGER` or text primary key is the table's key; a composite one, or
//! one holding NULLs, leaves the table without a key. A single-column
//! foreign key to another imported table's key is a reference
//! ([`crate::schema::Field::references`]). `NOT NULL` carries over, and so
//! does a constant `DEFAULT` (`CURRENT_TIMESTAMP` is `"now"`). Views,
//! triggers, `CHECK` and `UNIQUE` constraints, collations and virtual
//! tables other than FTS tables with their own text are not imported; the
//! plan's notes list what was left out.
//!
//! [`export`] writes a database's tables to a new SQLite file, one SQLite
//! table each, rows read from one snapshot:
//!
//! | Strata field | SQLite column |
//! |---|---|
//! | `KEYWORD`, `TEXT` | `TEXT` (the key: `TEXT PRIMARY KEY`) |
//! | `INT` | `INTEGER` (the key: `INTEGER PRIMARY KEY`, the rowid) |
//! | `FLOAT` | `REAL` |
//! | `BOOL` | `BOOLEAN`, 0 or 1 |
//! | `TIMESTAMP` | `TIMESTAMP`, UTC text as SQLite writes it (`2026-10-01 12:30:05.250`) |
//! | `VECTOR(n)` | `F32_BLOB(n)`: little-endian float32, as libSQL and sqlite-vec read them |
//! | multi-valued `KEYWORD` or `INT`, `EDGES`, `SPARSE`, `MEDIA` | `JSON` text |
//!
//! References become `REFERENCES` clauses, `NOT NULL` and `DEFAULT`
//! carry over, single-valued indexed fields get B-tree indexes, and the
//! `TEXT` fields of a table an FTS5 index over them (`<table>_fts`, with
//! triggers that keep it in step). What SQLite cannot hold is left
//! behind: the vector indexes (HNSW, IVF, codes), Strata's analyzers (FTS5's
//! `porter` tokenizer stands in for `english`, `unicode61` for `simple`),
//! BM25 and fused ranking, embedding models, graph indexes over `EDGES`,
//! and generated fields' definitions (their values are exported). Each
//! table's Strata schema is kept in [`META_TABLE`], so that an import of
//! the file restores the schemas as they were.

mod export;
mod import;

pub use export::{export, ExportOptions, Exported, TableExport};
pub use import::{import, import_new, plan, schemas_from_json, Given, ImportOptions, Imported, Plan, Source, TableImport, TablePlan};

/// The table an export keeps each Strata table's schema in (`name`,
/// `position`, `schema` as short JSON), which an import of the file reads.
pub const META_TABLE: &str = "_strata_tables";

/// Whether `path` starts as a SQLite database file does.
pub fn is_sqlite_file(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 16];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && magic == *MAGIC
}

/// The first 16 bytes of every SQLite database file.
pub const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// `name`, or `name_2`, `name_3`..: the first not in `used` (in any case),
/// which it joins.
fn distinct(name: String, used: &mut std::collections::HashSet<String>) -> String {
    let mut n = name.clone();
    let mut i = 2;
    while !used.insert(n.to_ascii_lowercase()) {
        n = format!("{name}_{i}");
        i += 1;
    }
    n
}

/// `name` as a SQL identifier.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `s` as a SQL string literal.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
