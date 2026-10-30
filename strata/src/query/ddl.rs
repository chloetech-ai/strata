//! `CREATE TABLE` and `DROP TABLE` (DESIGN.md §3.7).
//!
//! ```sql
//! CREATE TABLE authors (
//!   id KEYWORD PRIMARY KEY,
//!   name TEXT,
//!   born INT,
//!   topics KEYWORD[],
//!   bio TEXT OPTIONS(positions = false),
//!   emb VECTOR OPTIONS(embed = 'name, bio', model = 'potion-retrieval-32M')
//! );
//! CREATE TABLE IF NOT EXISTS writes (
//!   id INT PRIMARY KEY,
//!   paper KEYWORD REFERENCES papers,
//!   author KEYWORD REFERENCES authors (id)
//! );
//! DROP TABLE IF EXISTS writes, authors;
//! ```
//!
//! Types are `TEXT`, `KEYWORD`, `INT` (or `INTEGER`, `BIGINT`), `FLOAT` (or
//! `DOUBLE`, `REAL`), `BOOL` (or `BOOLEAN`), `TIMESTAMP` (microseconds,
//! UTC; or `TIMESTAMPTZ`, `DATETIME`), `VECTOR(dimensions)`, `SPARSE`,
//! `EDGES` and `MEDIA` (a file's reference, [`crate::media`]; `OPTIONS(kind
//! = 'image')`, `'audio'`, `'video'` or `'pdf'` says what file); `KEYWORD[]`
//! and `INT[]` hold lists. A table with a BOOL, TIMESTAMP or MEDIA column
//! makes the database format 3 ([`crate::storage::manifest`]). `OPTIONS(...)` takes the options of
//! the schema's short JSON form ([`crate::schema::Schema::from_json`]):
//! `positions`, `stored` and `indexed` (true or false), `from` (the fields
//! a generated TEXT field joins), `analyzer` (`'english'`, the default, or
//! `'simple'`; another than `english` makes the database format 3), `index` (`'hnsw'` with `m` and
//! `ef_construction`, `'flat'`, or `'ivf'` with `nlist`), `codes` (`'sq8'`
//! or `'binary'`), `dim`, and `embed` with `model` (the fields a VECTOR
//! field is the model's embedding of). Field lists are a string, `'title,
//! abstract'`, or `ARRAY['title', 'abstract']`. A KEYWORD[] column with
//! `semantic_id = 'emb'` and `codebook = 'name'` holds the semantic IDs of
//! VECTOR column `emb`'s vectors ([`crate::sid`]; feature `semantic_id`):
//! `sid KEYWORD[] OPTIONS(semantic_id = 'emb', codebook = 'items-rq')`.
//!
//! The key is a column's `PRIMARY KEY` or a `PRIMARY KEY (column)`
//! constraint. `REFERENCES table [(key)]` on a column, or `FOREIGN KEY
//! (column) REFERENCES table [(key)]`, records that a KEYWORD or INT
//! column holds primary keys of `table` ([`crate::schema::Field::references`]);
//! it is not enforced. `NOT NULL` refuses writes that leave the column
//! NULL, and `DEFAULT value` (a constant, or `CURRENT_TIMESTAMP` for a
//! TIMESTAMP) is what a write that leaves the column out stores; either
//! makes the database format 3 ([`crate::schema::Field::not_null`],
//! [`crate::schema::Field::default`]).
//!
//! `OPTIONS(...)` after the columns takes the table's options: `shard_by`,
//! the column that places its rows on shards instead of their key
//! ([`crate::schema::Schema::with_shard_by`]); `cluster_by`, the column
//! that orders each segment's rows ([`crate::schema::Schema::with_cluster_by`]);
//! and `retain`, how long rows are kept after the time in `retain_by`
//! (default: the table's only TIMESTAMP; [`crate::schema::Schema::with_retain`]):
//!
//! ```sql
//! CREATE TABLE events (
//!   id INT PRIMARY KEY,
//!   usr KEYWORD NOT NULL,
//!   item KEYWORD REFERENCES items,
//!   ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP
//! ) OPTIONS(shard_by = 'usr', cluster_by = 'usr', retain = '400 days');
//! ```
//!
//! Every other clause and column option (`UNIQUE`, `CHECK`, `ON DELETE`,
//! constraint names, `AS SELECT`, ...) is an error, never ignored.

use serde_json::{json, Map, Value as J};
use sqlparser::ast::helpers::stmt_create_table::CreateTableBuilder;
use sqlparser::ast::{self as sql, ColumnOption, DataType, Expr, TableConstraint};

use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::storage::DropOptions;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// The forms this module binds, for error messages.
const CREATE_FORM: &str = "CREATE TABLE [IF NOT EXISTS] name (column TYPE [PRIMARY KEY] [NOT NULL] [DEFAULT value] [REFERENCES table] [OPTIONS(...)], ..., [PRIMARY KEY (column)], [FOREIGN KEY (column) REFERENCES table]) [OPTIONS(shard_by = 'column', cluster_by = 'column', retain = '400 days', retain_by = 'column', edges = 'src, dst, w')]";
/// The options of a table, in `OPTIONS(...)` after its columns.
const TABLE_OPTIONS: [&str; 5] = ["shard_by", "cluster_by", "retain", "retain_by", "edges"];
const TYPES: &str = "TEXT, KEYWORD, INT, FLOAT, BOOL, TIMESTAMP, VECTOR(dimensions), SPARSE, EDGES or MEDIA, and KEYWORD[] or INT[] for lists";

/// The table a `CREATE TABLE` statement makes, and whether it says `IF NOT
/// EXISTS`. `tables` are the database's tables, which references name.
pub(crate) fn create_table(ct: &sql::CreateTable, tables: &[&Schema]) -> Result<(Schema, bool)> {
    // Every clause but the name, columns, constraints and IF NOT EXISTS
    // must be as a plain CREATE TABLE leaves it.
    let table_options = match &ct.table_options {
        sql::CreateTableOptions::Options(o) => o.clone(),
        _ => Vec::new(),
    };
    let mut plain = CreateTableBuilder::new(ct.name.clone()).columns(ct.columns.clone()).constraints(ct.constraints.clone()).if_not_exists(ct.if_not_exists);
    if !table_options.is_empty() {
        plain = plain.table_options(sql::CreateTableOptions::Options(table_options.clone()));
    }
    let mut plain = plain.build();
    if let sql::Statement::CreateTable(p) = &mut plain {
        if ct.hive_formats.as_ref().is_none_or(|h| *h == sql::HiveFormat::default()) {
            p.hive_formats = ct.hive_formats.clone();
        }
    }
    if plain != sql::Statement::CreateTable(ct.clone()) {
        return Err(err(format!("this form of CREATE TABLE is not supported; the form is {CREATE_FORM}")));
    }
    let name = single_name(&ct.name)?;
    if ct.columns.is_empty() {
        return Err(err("CREATE TABLE needs columns"));
    }
    let mut key: Option<String> = None;
    let mut set_key = |c: &str| -> Result<()> {
        if key.is_some() {
            return Err(err("a table has one primary key, of one column"));
        }
        key = Some(c.to_string());
        Ok(())
    };
    // (column, table, the key it names if it names one)
    let mut refs: Vec<(String, String, Option<String>)> = Vec::new();
    let mut fields = Vec::with_capacity(ct.columns.len());
    for c in &ct.columns {
        let mut opts = Map::new();
        let (mut null, mut not_null, mut default) = (false, false, None);
        for o in &c.options {
            if let Some(n) = &o.name {
                return Err(err(format!("column {}: constraint names ({n}) are not supported", c.name.value)));
            }
            match &o.option {
                ColumnOption::Null => null = true,
                ColumnOption::NotNull => not_null = true,
                ColumnOption::Default(e) => {
                    if default.replace(default_value(e).map_err(|m| err(format!("column {}: {m}", c.name.value)))?).is_some() {
                        return Err(err(format!("column {}: DEFAULT is given twice", c.name.value)));
                    }
                }
                ColumnOption::Unique { is_primary: true, characteristics: None } => set_key(&c.name.value)?,
                ColumnOption::ForeignKey { foreign_table, referred_columns, on_delete, on_update, characteristics } => {
                    refs.push((c.name.value.clone(), reference(foreign_table, referred_columns, on_delete, on_update, characteristics)?, referred_columns.first().map(|k| k.value.clone())));
                }
                ColumnOption::Options(kv) => {
                    for o in kv {
                        let sql::SqlOption::KeyValue { key, value } = o else { return Err(err(format!("column {}: OPTIONS takes key = value pairs, not {o}", c.name.value))) };
                        if opts.insert(key.value.to_ascii_lowercase(), option_value(value)?).is_some() {
                            return Err(err(format!("column {}: option {} is given twice", c.name.value, key.value)));
                        }
                    }
                }
                ColumnOption::Unique { is_primary: false, .. } => return Err(err(format!("column {}: UNIQUE is not supported (the primary key is unique)", c.name.value))),
                o => return Err(err(format!("column {}: {o} is not supported", c.name.value))),
            }
        }
        if null && not_null {
            return Err(err(format!("column {}: NULL and NOT NULL", c.name.value)));
        }
        let mut f = field(&c.name.value, &c.data_type, opts)?;
        if not_null {
            f["not_null"] = json!(true);
        }
        if let Some(d) = default {
            f["default"] = d;
        }
        fields.push(f);
    }
    for t in &ct.constraints {
        match t {
            TableConstraint::PrimaryKey { name: None, index_name: None, index_type: None, columns, index_options, characteristics: None } if index_options.is_empty() => {
                let [c] = columns.as_slice() else { return Err(err("a table has one primary key, of one column")) };
                let sql::IndexColumn { column: sql::OrderByExpr { expr: Expr::Identifier(id), options: sql::OrderByOptions { asc: None, nulls_first: None }, with_fill: None }, operator_class: None } = c else {
                    return Err(err(format!("PRIMARY KEY ({c}) names a column, and only that")));
                };
                set_key(&id.value)?;
            }
            TableConstraint::ForeignKey { name: None, index_name: None, columns, foreign_table, referred_columns, on_delete, on_update, characteristics } => {
                let [c] = columns.as_slice() else { return Err(err("FOREIGN KEY names one column")) };
                refs.push((c.value.clone(), reference(foreign_table, referred_columns, on_delete, on_update, characteristics)?, referred_columns.first().map(|k| k.value.clone())));
            }
            t => return Err(err(format!("{t} is not supported; the form is {CREATE_FORM}"))),
        }
    }
    for (column, table, _) in &refs {
        let f = fields.iter_mut().find(|f| f["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(column))).ok_or_else(|| err(format!("no such column: {column}")))?;
        if f.get("references").is_some() {
            return Err(err(format!("column {column} references a table twice")));
        }
        f["references"] = json!(table);
    }
    let mut short = json!({"table": name, "key": key, "fields": fields});
    for o in &table_options {
        let sql::SqlOption::KeyValue { key, value } = o else { return Err(err(format!("the table's OPTIONS take key = value pairs, not {o}"))) };
        let k = key.value.to_ascii_lowercase();
        if !TABLE_OPTIONS.contains(&k.as_str()) {
            return Err(err(format!("{} is not an option of a table (its options: {})", key.value, TABLE_OPTIONS.join(", "))));
        }
        if short.get(&k).is_some() {
            return Err(err(format!("the table's option {} is given twice", key.value)));
        }
        match option_value(value)? {
            v @ J::String(_) => short[k] = v,
            _ if k == "retain" => return Err(err("the table's option retain is a period, in quotes: retain = '400 days'")),
            _ if k == "edges" => return Err(err("the table's option edges names its source, target and weight columns, in quotes: edges = 'src, dst, w'")),
            _ => return Err(err(format!("the table's option {k} names a column, in quotes: {k} = 'usr'"))),
        }
    }
    let schema = Schema::from_json_value(&short)?;
    // A reference names the primary key of its table, if it names a column.
    for (column, table, named) in &refs {
        let Some(named) = named else { continue };
        let target = if table.eq_ignore_ascii_case(&schema.table) { Some(&schema) } else { tables.iter().copied().find(|t| t.table.eq_ignore_ascii_case(table)) };
        let Some(target) = target else { return Err(err(format!("column {column} references {table}, which is not a table"))) };
        let pk = target.primary_key.map(|k| target.fields[k].name.as_str());
        if !pk.is_some_and(|k| k.eq_ignore_ascii_case(named)) {
            return Err(err(format!(
                "column {column} references {table} ({named}): a reference names its table's primary key, {}",
                pk.map_or(format!("and {} has none", target.table), |k| format!("which is {k}"))
            )));
        }
    }
    Ok((schema, ct.if_not_exists))
}

/// The table a reference names, checked for the clauses references do not
/// take (they are recorded, not enforced).
fn reference(
    table: &sql::ObjectName,
    columns: &[sql::Ident],
    on_delete: &Option<sql::ReferentialAction>,
    on_update: &Option<sql::ReferentialAction>,
    characteristics: &Option<sql::ConstraintCharacteristics>,
) -> Result<String> {
    if on_delete.is_some() || on_update.is_some() || characteristics.is_some() {
        return Err(err("references are recorded, not enforced, so they take no ON DELETE, ON UPDATE or DEFERRABLE"));
    }
    if columns.len() > 1 {
        return Err(err(format!("REFERENCES {table} names one column, the table's primary key")));
    }
    single_name(table)
}

fn single_name(n: &sql::ObjectName) -> Result<String> {
    match n.0.as_slice() {
        [sql::ObjectNamePart::Identifier(id)] => Ok(id.value.clone()),
        _ => Err(err(format!("a table name is one identifier, not {n}"))),
    }
}

/// A column as a field of the short JSON form.
fn field(name: &str, ty: &DataType, mut opts: Map<String, J>) -> Result<J> {
    let bad = |m: String| err(format!("column {name}: {m}"));
    let (ty, multi, dim) = column_type(ty).map_err(bad)?;
    let allowed: &[&str] = match ty {
        "text" => &["positions", "stored", "indexed", "from", "analyzer"],
        "vector" => &["stored", "indexed", "dim", "index", "m", "ef_construction", "nlist", "codes", "embed", "model"],
        "media" => &["stored", "indexed", "kind"],
        "keyword" if multi => &["stored", "indexed", "semantic_id", "codebook"],
        _ => &["stored", "indexed"],
    };
    if !allowed.contains(&"semantic_id") && (opts.contains_key("semantic_id") || opts.contains_key("codebook")) {
        return Err(bad("semantic_id and codebook are options of a KEYWORD[] column".into()));
    }
    if let Some(k) = opts.keys().find(|k| !allowed.contains(&k.as_str())) {
        let known = ["positions", "stored", "indexed", "from", "analyzer", "index", "m", "ef_construction", "nlist", "codes", "dim", "embed", "model", "kind", "semantic_id", "codebook"];
        return Err(bad(if known.contains(&k.as_str()) {
            format!("{k} is not an option of a {} column (its options: {})", ty.to_uppercase(), allowed.join(", "))
        } else {
            format!("unknown option {k} (known: {})", known.join(", "))
        }));
    }
    let mut f = Map::new();
    f.insert("name".into(), json!(name));
    f.insert("type".into(), json!(ty));
    if multi {
        f.insert("multi".into(), json!(true));
    }
    if let Some(d) = dim {
        f.insert("dim".into(), json!(d));
    }
    // The vector index and the embedding are objects in the JSON form.
    let index = opts.remove("index");
    let hnsw = [("m", opts.remove("m")), ("ef_construction", opts.remove("ef_construction"))];
    let nlist = opts.remove("nlist");
    let kind = index.as_ref().and_then(|i| i.as_str()).map(|i| i.to_ascii_lowercase());
    match kind.as_deref() {
        None if index.is_some() => return Err(bad("index is 'hnsw', 'flat' or 'ivf'".into())),
        None | Some("hnsw") if hnsw.iter().any(|(_, v)| v.is_some()) => {
            let mut p = Map::new();
            for (k, v) in hnsw {
                if let Some(v) = v {
                    p.insert(k.into(), positive(&v).map_err(|m| bad(format!("{k} {m}")))?);
                }
            }
            f.insert("index".into(), json!({"hnsw": p}));
        }
        None => {}
        Some("hnsw") | Some("flat") => {
            if hnsw.iter().any(|(_, v)| v.is_some()) {
                return Err(bad("m and ef_construction are options of index = 'hnsw'".into()));
            }
            f.insert("index".into(), json!(kind));
        }
        Some("ivf") => {
            let n = nlist.as_ref().ok_or_else(|| bad("index = 'ivf' needs nlist".into()))?;
            f.insert("index".into(), json!({"ivf": {"nlist": positive(n).map_err(|m| bad(format!("nlist {m}")))?}}));
        }
        Some(k) => return Err(bad(format!("index is 'hnsw', 'flat' or 'ivf', not '{k}'"))),
    }
    if nlist.is_some() && kind.as_deref() != Some("ivf") {
        return Err(bad("nlist is an option of index = 'ivf'".into()));
    }
    match (opts.remove("embed"), opts.remove("model")) {
        (None, None) => {}
        (Some(from), Some(model)) if model.is_string() => {
            f.insert("embed".into(), json!({"from": names(&from).map_err(|m| bad(format!("embed {m}")))?, "model": model}));
        }
        _ => return Err(bad("an embedded vector takes embed = 'fields' and model = 'name'".into())),
    }
    if let Some(from) = opts.remove("from") {
        f.insert("from".into(), json!(names(&from).map_err(|m| bad(format!("from {m}")))?));
    }
    match (opts.remove("semantic_id"), opts.remove("codebook")) {
        (None, None) => {}
        (Some(J::String(from)), Some(J::String(codebook))) => {
            f.insert("semantic_id".into(), json!({"from": from, "codebook": codebook}));
        }
        _ => return Err(bad("a semantic-ID column takes semantic_id = 'vector column' and codebook = 'name'".into())),
    }
    for (k, v) in opts {
        match k.as_str() {
            "positions" | "stored" | "indexed" if !v.is_boolean() => return Err(bad(format!("{k} is true or false"))),
            "dim" => {
                positive(&v).map_err(|m| bad(format!("dim {m}")))?;
            }
            _ => {}
        }
        if f.insert(k.clone(), v).is_some() {
            return Err(bad(format!("{k} is given twice")));
        }
    }
    Ok(J::Object(f))
}

/// A column type: the short form's type name, whether it holds lists, and
/// a vector's dimensions.
fn column_type(t: &DataType) -> std::result::Result<(&'static str, bool, Option<u64>), String> {
    if let DataType::Array(a) = t {
        let sql::ArrayElemTypeDef::SquareBracket(inner, None) = a else { return Err(format!("{t} is not a type here: {TYPES}")) };
        return match column_type(inner)? {
            (ty @ ("keyword" | "int"), false, None) => Ok((ty, true, None)),
            _ => Err(format!("{t}: only KEYWORD[] and INT[] hold lists")),
        };
    }
    if let DataType::Custom(name, args) = t {
        let n = name.to_string().to_ascii_lowercase();
        return match (n.as_str(), args.as_slice()) {
            ("keyword", []) => Ok(("keyword", false, None)),
            ("sparse", []) => Ok(("sparse", false, None)),
            ("edges", []) => Ok(("edges", false, None)),
            ("media", []) => Ok(("media", false, None)),
            ("vector", []) => Ok(("vector", false, None)),
            ("vector", [d]) => match d.parse::<u64>() {
                Ok(d) if d > 0 => Ok(("vector", false, Some(d))),
                _ => Err(format!("VECTOR({d}): the dimensions are a positive number")),
            },
            _ => Err(format!("{t} is not a type here: {TYPES}")),
        };
    }
    match t.to_string().to_ascii_uppercase().as_str() {
        "TEXT" => Ok(("text", false, None)),
        "INT" | "INTEGER" | "BIGINT" | "INT64" => Ok(("int", false, None)),
        "BOOL" | "BOOLEAN" => Ok(("bool", false, None)),
        "TIMESTAMP" | "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" | "DATETIME" => Ok(("timestamp", false, None)),
        "DATE" => Err(format!("{t} is not a type here: a TIMESTAMP holds dates (as midnight UTC) and times")),
        "FLOAT" | "DOUBLE" | "REAL" | "DOUBLE PRECISION" | "FLOAT64" => Ok(("float", false, None)),
        s if s.starts_with("VARCHAR") || s.starts_with("CHAR") || s.starts_with("STRING") => {
            Err(format!("{t} is not a type here: use TEXT for text searched by its words, KEYWORD for exact values"))
        }
        _ => Err(format!("{t} is not a type here: {TYPES}")),
    }
}

/// A column's `DEFAULT`, as the schema's JSON form keeps it: a constant
/// (`'open'`, `0`, `-1.5`, `TRUE`, `'2026-01-01'`, `NULL`), or the time
/// of each write as `"now"` (`CURRENT_TIMESTAMP`, `now()`,
/// `LOCALTIMESTAMP`), which a TIMESTAMP column takes.
fn default_value(e: &Expr) -> std::result::Result<J, String> {
    let now = |n: &str| matches!(n.to_ascii_lowercase().as_str(), "current_timestamp" | "now" | "localtimestamp");
    let no_args = |f: &sql::Function| match &f.args {
        sql::FunctionArguments::None => true,
        sql::FunctionArguments::List(l) => l.args.is_empty(),
        _ => false,
    };
    match e {
        Expr::Nested(x) => default_value(x),
        Expr::Value(v) => match &v.value {
            sql::Value::SingleQuotedString(s) | sql::Value::DoubleQuotedString(s) => Ok(json!(s)),
            sql::Value::Boolean(b) => Ok(json!(b)),
            sql::Value::Null => Ok(J::Null),
            sql::Value::Number(n, _) => n.parse::<i64>().map(|n| json!(n)).or_else(|_| n.parse::<f64>().map(|x| json!(x))).map_err(|_| format!("DEFAULT {n} is not a number")),
            _ => Err(format!("DEFAULT {e} is not a constant")),
        },
        Expr::UnaryOp { op: sql::UnaryOperator::Minus, expr } => match default_value(expr)? {
            J::Number(n) if n.is_i64() => Ok(json!(-n.as_i64().unwrap_or(0))),
            J::Number(n) if n.is_f64() => Ok(json!(-n.as_f64().unwrap_or(0.0))),
            _ => Err(format!("DEFAULT {e} is not a constant")),
        },
        Expr::Function(f) if now(&f.name.to_string()) && no_args(f) => Ok(json!("now")),
        Expr::Identifier(id) if id.quote_style.is_none() && now(&id.value) => Ok(json!("now")),
        _ => Err(format!("DEFAULT {e} is not a constant (or CURRENT_TIMESTAMP for a TIMESTAMP)")),
    }
}

/// An option's value: a string, number or boolean literal, or an `ARRAY`
/// of strings.
fn option_value(e: &Expr) -> Result<J> {
    match e {
        Expr::Value(v) => match &v.value {
            sql::Value::SingleQuotedString(s) | sql::Value::DoubleQuotedString(s) => Ok(json!(s)),
            sql::Value::Boolean(b) => Ok(json!(b)),
            sql::Value::Number(n, _) => n.parse::<u64>().map(|n| json!(n)).map_err(|_| err(format!("option value {n} is not a whole number"))),
            _ => Err(err(format!("option value {e} is not a string, number or boolean"))),
        },
        Expr::Array(a) => a.elem.iter().map(option_value).collect::<Result<Vec<_>>>().map(J::Array),
        _ => Err(err(format!("option value {e} is not a string, number or boolean"))),
    }
}

/// Field names: a string of names separated by commas, or an array.
fn names(v: &J) -> std::result::Result<Vec<String>, String> {
    let out: Vec<String> = match v {
        J::String(s) => s.split(',').map(|n| n.trim().to_string()).collect(),
        J::Array(xs) => xs.iter().map(|x| x.as_str().map(|s| s.trim().to_string()).ok_or_else(|| "lists field names".to_string())).collect::<std::result::Result<_, _>>()?,
        _ => return Err("lists field names, as 'title, abstract'".into()),
    };
    if out.iter().any(|n| n.is_empty()) {
        return Err("lists field names, as 'title, abstract'".into());
    }
    Ok(out)
}

fn positive(v: &J) -> std::result::Result<J, String> {
    match v.as_u64() {
        Some(n) if n > 0 => Ok(json!(n)),
        _ => Err("is a positive number".into()),
    }
}

/// The tables a `DROP TABLE` statement drops, and how.
pub(crate) fn drop_table(stmt: &sql::Statement) -> Result<(Vec<String>, DropOptions)> {
    let sql::Statement::Drop { object_type, if_exists, names, cascade, restrict: _, purge, temporary, table } = stmt else {
        return Err(err("not a DROP statement"));
    };
    if *object_type != sql::ObjectType::Table {
        return Err(err(format!("DROP {object_type} is not supported, only DROP TABLE")));
    }
    if *purge || *temporary || table.is_some() {
        return Err(err(format!("this form of DROP TABLE is not supported: {stmt}")));
    }
    let tables = names.iter().map(single_name).collect::<Result<Vec<_>>>()?;
    Ok((tables, DropOptions { cascade: *cascade, if_exists: *if_exists }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse::parse;
    use crate::schema::{FieldType, MediaKind, VectorCodes, VectorIndexKind};

    fn create(text: &str, tables: &[&Schema]) -> Result<(Schema, bool)> {
        let stmts = parse(text)?;
        let sql::Statement::CreateTable(ct) = &stmts[0] else { panic!("{text}") };
        create_table(ct, tables)
    }

    fn papers() -> Schema {
        Schema::new("papers", vec![crate::schema::Field::new("id", FieldType::Keyword), crate::schema::Field::new("n", FieldType::Int)], Some("id")).unwrap()
    }

    #[test]
    fn creates_every_type_and_option() {
        let p = papers();
        let (s, ine) = create(
            "CREATE TABLE IF NOT EXISTS authors (
               id KEYWORD PRIMARY KEY, name TEXT, bio TEXT OPTIONS(positions = false), born INTEGER OPTIONS(stored = false),
               score DOUBLE, topics KEYWORD[], ns INT[], links EDGES REFERENCES authors, sp SPARSE,
               papers KEYWORD[] REFERENCES papers (id), both_ TEXT OPTIONS(from = 'name, topics'),
               emb VECTOR(8) OPTIONS(index = 'ivf', nlist = 4, codes = 'binary'),
               e2 VECTOR OPTIONS(embed = ARRAY['name'], model = 'hash:16', m = 8),
               hidden INT OPTIONS(indexed = false), flat VECTOR(3) OPTIONS(index = 'flat'))",
            &[&p],
        )
        .unwrap();
        assert!(ine);
        assert_eq!(s.table, "authors");
        assert_eq!(s.primary_key, Some(0));
        let f = |n: &str| s.field(n).unwrap();
        assert_eq!(f("name").ty, FieldType::Text { positions: true });
        assert!(f("bio").stored && f("bio").ty == FieldType::Text { positions: false });
        assert_eq!((&f("born").ty, &f("score").ty), (&FieldType::Int, &FieldType::Float));
        assert!(!f("born").stored);
        assert!(f("topics").multi && f("ns").multi && f("ns").ty == FieldType::Int);
        assert_eq!(f("links").references.as_deref(), Some("authors"));
        assert_eq!(f("papers").references.as_deref(), Some("papers"));
        assert_eq!(f("both_").generated_from, vec!["name", "topics"]);
        assert_eq!(f("emb").ty, FieldType::Vector { dim: 8, index: VectorIndexKind::Ivf { nlist: 4 } });
        assert_eq!(f("emb").codes, VectorCodes::Binary);
        assert_eq!(f("e2").ty, FieldType::Vector { dim: 16, index: VectorIndexKind::Hnsw { m: 8, ef_construction: 200 } });
        assert_eq!(f("e2").embed.as_ref().map(|e| (e.from.clone(), e.model.as_str())), Some((vec!["name".to_string()], "hash:16")));
        assert!(!f("hidden").indexed);
        assert_eq!(f("flat").ty, FieldType::Vector { dim: 3, index: VectorIndexKind::Flat });

        let (s, ine) = create("CREATE TABLE w (a INT, p KEYWORD, PRIMARY KEY (a), FOREIGN KEY (p) REFERENCES papers)", &[&p]).unwrap();
        assert!(!ine);
        assert_eq!(s.primary_key, Some(0));
        assert_eq!(s.fields[1].references.as_deref(), Some("papers"));
        // No key is allowed, as in the JSON form.
        assert_eq!(create("CREATE TABLE log (line TEXT NULL)", &[]).unwrap().0.primary_key, None);
    }

    #[test]
    fn media_columns_name_their_kind() {
        let (s, _) = create("CREATE TABLE photos (id KEYWORD PRIMARY KEY, image MEDIA OPTIONS(kind = 'image'), any_file MEDIA, scan MEDIA OPTIONS(kind = 'PDF', indexed = false))", &[]).unwrap();
        let f = |n: &str| s.field(n).unwrap();
        assert_eq!((f("image").media, f("any_file").media, f("scan").media), (Some(MediaKind::Image), Some(MediaKind::Any), Some(MediaKind::Pdf)));
        assert!(f("image").ty == FieldType::Keyword && f("image").stored && f("image").indexed && !f("scan").indexed);
        assert_eq!(f("image").type_name(), "MEDIA");
        assert_eq!(s.features().collect::<Vec<_>>(), ["media", "media", "media"]);
        assert_eq!(s.to_short_json()["fields"][1], serde_json::json!({"name": "image", "type": "media", "kind": "image"}));
        assert_eq!(s.to_short_json()["fields"][2], serde_json::json!({"name": "any_file", "type": "media"}));
        assert_eq!(Schema::from_json_value(&s.to_short_json()).unwrap(), s);
        assert_eq!(serde_json::from_str::<Schema>(&serde_json::to_string(&s).unwrap()).unwrap(), s);
        for (bad, says) in [
            ("CREATE TABLE t (a MEDIA OPTIONS(kind = 'smell'))", "kind is"),
            ("CREATE TABLE t (a KEYWORD OPTIONS(kind = 'image'))", "not an option of a KEYWORD"),
            ("CREATE TABLE t (a MEDIA OPTIONS(stored = false))", "is stored"),
            ("CREATE TABLE t (a MEDIA PRIMARY KEY)", "neither the key"),
            ("CREATE TABLE t (a MEDIA DEFAULT 'x')", "given a default"),
            ("CREATE TABLE t (a MEDIA REFERENCES t)", "neither the key, a reference"),
            ("CREATE TABLE t (a MEDIA[])", "only KEYWORD[] and INT[]"),
            ("CREATE TABLE t (a MEDIA, b TEXT OPTIONS(from = 'a'))", "not a source of text"),
            ("CREATE TABLE t (a MEDIA, e VECTOR OPTIONS(embed = 'a', model = 'hash:8'))", "is of kind image"),
            ("CREATE TABLE t (id INT PRIMARY KEY, a MEDIA NOT NULL) OPTIONS(shard_by = 'a')", "KEYWORD or INT"),
        ] {
            let e = create(bad, &[]).unwrap_err().to_string();
            assert!(e.contains(says), "{bad}: {e}");
        }
    }

    #[test]
    fn a_table_is_placed_by_a_column() {
        let (s, _) = create("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, ts INT) OPTIONS(shard_by = 'USR')", &[]).unwrap();
        assert_eq!(s.shard_by.as_deref(), Some("usr"));
        assert_eq!(s.shard_by_field(), Some(1));
        assert_eq!(s.features().collect::<Vec<_>>(), ["not_null", "shard_by"]);
        assert_eq!(s.to_short_json()["shard_by"], "usr");
        assert_eq!(Schema::from_json_value(&s.to_short_json()).unwrap(), s);
        assert_eq!(serde_json::from_str::<Schema>(&serde_json::to_string(&s).unwrap()).unwrap(), s);
        // A table without the option serializes as before it existed.
        let plain = create("CREATE TABLE t (id INT PRIMARY KEY)", &[]).unwrap().0;
        assert!(!serde_json::to_string(&plain).unwrap().contains("shard_by") && plain.to_short_json().get("shard_by").is_none());
        for (bad, says) in [
            ("CREATE TABLE t (id INT PRIMARY KEY, u KEYWORD) OPTIONS(shard_by = 'u')", "NOT NULL"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u KEYWORD NOT NULL) OPTIONS(shard_by = 'v')", "not a field"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u KEYWORD NOT NULL) OPTIONS(shard_by = 'id')", "by their key already"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u TEXT NOT NULL) OPTIONS(shard_by = 'u')", "KEYWORD or INT"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u KEYWORD[] NOT NULL) OPTIONS(shard_by = 'u')", "single-valued"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u TIMESTAMP NOT NULL) OPTIONS(shard_by = 'u')", "KEYWORD or INT"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u KEYWORD NOT NULL) OPTIONS(shard_by = 3)", "names a column"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u KEYWORD NOT NULL) OPTIONS(color = 'u')", "not an option of a table"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u KEYWORD NOT NULL) OPTIONS(shard_by = 'u', shard_by = 'u')", "given twice"),
            ("CREATE TABLE t (id INT PRIMARY KEY, u KEYWORD NOT NULL) WITH (shard_by = 'u')", "not supported"),
        ] {
            let e = create(bad, &[]).unwrap_err().to_string();
            assert!(e.contains(says), "{bad}: {e}");
        }
    }

    #[test]
    fn refuses_what_it_does_not_handle() {
        let p = papers();
        for bad in [
            "CREATE TABLE t (a INT NULL NOT NULL)",
            "CREATE TABLE t (a INT DEFAULT 1 DEFAULT 2)",
            "CREATE TABLE t (a INT DEFAULT 1 + 1)",
            "CREATE TABLE t (a INT DEFAULT 'one')",
            "CREATE TABLE t (a INT PRIMARY KEY DEFAULT 1)",
            "CREATE TABLE t (a INT, b INT DEFAULT CURRENT_TIMESTAMP)",
            "CREATE TABLE t (a INT, b KEYWORD[] DEFAULT 'x')",
            "CREATE TABLE t (a INT UNIQUE)",
            "CREATE TABLE t (a INT CHECK (a > 0))",
            "CREATE TABLE t (a INT PRIMARY KEY, b INT PRIMARY KEY)",
            "CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b))",
            "CREATE TABLE t (a INT CONSTRAINT k PRIMARY KEY)",
            "CREATE TABLE t (a VARCHAR(10))",
            "CREATE TABLE t (a TEXT[])",
            "CREATE TABLE t (a FLOAT[])",
            "CREATE TABLE t (a VECTOR(0))",
            "CREATE TABLE t (a BLOB)",
            "CREATE TABLE t (a KEYWORD REFERENCES papers (n))",
            "CREATE TABLE t (a KEYWORD REFERENCES papers ON DELETE CASCADE)",
            "CREATE TABLE t (a KEYWORD REFERENCES nope (id))",
            "CREATE TABLE t (a KEYWORD REFERENCES papers (id, n))",
            "CREATE TABLE t (a TEXT REFERENCES papers)",
            "CREATE TABLE t (a KEYWORD REFERENCES papers, FOREIGN KEY (a) REFERENCES papers)",
            "CREATE TABLE t (a INT PRIMARY KEY, b KEYWORD REFERENCES t (b))",
            "CREATE TABLE t (a VECTOR(4) OPTIONS(index = 'ivf'))",
            "CREATE TABLE t (a VECTOR(4) OPTIONS(nlist = 4))",
            "CREATE TABLE t (a VECTOR(4) OPTIONS(index = 'lsh'))",
            "CREATE TABLE t (a VECTOR(4) OPTIONS(model = 'hash:4'))",
            "CREATE TABLE t (a TEXT OPTIONS(stored = 1))",
            "CREATE TABLE t (a TEXT OPTIONS(color = 'red'))",
            "CREATE TABLE t (a TEXT OPTIONS(index = 'flat'))",
            "CREATE TABLE t (a KEYWORD OPTIONS(positions = false))",
            "CREATE TABLE t (a INT PRIMARY KEY, PRIMARY KEY (a DESC))",
            "CREATE TABLE t (a INT OPTIONS(stored = false, stored = true))",
            // Merges rebuild a TEXT index from the stored text.
            "CREATE TABLE t (a TEXT OPTIONS(stored = false))",
            "CREATE TABLE t (a INT, A INT)",
            "CREATE TABLE t (a INT) WITH (fillfactor = 70)",
            "CREATE TABLE t AS SELECT 1",
            "CREATE TEMPORARY TABLE t (a INT)",
            "CREATE OR REPLACE TABLE t (a INT)",
            "CREATE TABLE s.t (a INT)",
            "CREATE TABLE t (a INT) COMMENT 'x'",
        ] {
            assert!(create(bad, &[&p]).is_err(), "{bad}");
        }
    }

    #[test]
    fn drops() {
        let drop = |t: &str| drop_table(&parse(t).unwrap()[0]);
        assert_eq!(drop("DROP TABLE a").unwrap(), (vec!["a".to_string()], DropOptions::default()));
        assert_eq!(drop("DROP TABLE IF EXISTS a, b CASCADE").unwrap(), (vec!["a".to_string(), "b".to_string()], DropOptions { cascade: true, if_exists: true }));
        assert_eq!(drop("DROP TABLE a RESTRICT").unwrap().1, DropOptions::default());
        assert!(drop("DROP VIEW a").is_err());
        assert!(drop("DROP TABLE s.a").is_err());
    }
}
