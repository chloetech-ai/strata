//! JSON fields with text (`attrs JSON OPTIONS(text = true)`): the strings
//! of each row's object, long ones too, are indexed as text, so `MATCH`
//! finds exactly the rows whose strings hold the words, and `bm25` scores
//! them as it scores a TEXT field holding the same strings, one a line.
//! Keys, numbers and booleans are not text. Checked against a model of the
//! rows in the memtable, after flushes, merges and reopening, after
//! updates and deletes, and on a column added to a table with rows.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::Value;

/// Words the analyser keeps as they are (no stem, no stopword).
const WORDS: [&str; 8] = ["zebra", "quokka", "lynx", "otter", "heron", "mango", "kiwi", "amber"];

fn seed() -> Schema {
    Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap()
}

fn opts() -> DbOptions {
    DbOptions { sync: false, layout: Layout::SingleFile, segment_max_docs: 120, ..DbOptions::default() }
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn ids(rows: impl IntoIterator<Item = Vec<Value>>) -> Vec<String> {
    let mut v: Vec<String> = rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

/// A few words, sometimes padded past the length a value is indexed by.
fn words(rng: &mut StdRng) -> String {
    let n = rng.gen_range(1..4);
    let s: Vec<&str> = (0..n).map(|_| WORDS[rng.gen_range(0..WORDS.len())]).collect();
    if rng.gen_ratio(1, 6) {
        return format!("{}{}", "padding ".repeat(40), s.join(" "));
    }
    s.join(" ")
}

/// Row `i`'s object: strings at a few paths, in arrays and below objects,
/// with numbers, booleans and keys named like the words around them.
fn object(rng: &mut StdRng) -> Option<Json> {
    if rng.gen_ratio(1, 10) {
        return None;
    }
    let mut m = serde_json::Map::new();
    if rng.gen_ratio(1, 2) {
        m.insert("title".into(), json!(words(rng)));
    }
    if rng.gen_ratio(1, 3) {
        m.insert("tags".into(), Json::Array((0..rng.gen_range(0..3)).map(|_| json!(words(rng))).collect()));
    }
    if rng.gen_ratio(1, 3) {
        m.insert("spec".into(), json!({"note": words(rng), "size": rng.gen_range(0..9)}));
    }
    // A key that is a word, holding no text.
    if rng.gen_ratio(1, 3) {
        m.insert(WORDS[rng.gen_range(0..WORDS.len())].into(), json!(rng.gen_range(0..5)));
    }
    if rng.gen_ratio(1, 5) {
        m.insert("ok".into(), json!(true));
    }
    Some(Json::Object(m))
}

/// The strings of an object, as the model sees them.
fn strings(j: &Json, out: &mut Vec<String>) {
    match j {
        Json::String(s) => out.push(s.clone()),
        Json::Array(xs) => xs.iter().for_each(|x| strings(x, out)),
        Json::Object(m) => m.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

struct Row {
    id: String,
    attrs: Option<Json>,
}

impl Row {
    fn has(&self, word: &str) -> bool {
        let mut s = Vec::new();
        if let Some(j) = &self.attrs {
            strings(j, &mut s);
        }
        s.iter().any(|x| x.split_whitespace().any(|w| w == word))
    }
    /// The TEXT a field holding the same strings would hold.
    fn text(&self) -> Option<String> {
        let v = self.attrs.as_ref().map(|j| Value::Str(j.to_string()))?;
        strata::json::text_of(&v).unwrap()
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Rows into `t` (attrs JSON with text) and the same strings into `u`'s
/// TEXT field.
fn fill(db: &Db, rng: &mut StdRng, from: usize, n: usize) -> Vec<Row> {
    let rows: Vec<Row> = (from..from + n).map(|i| Row { id: format!("r{i:04}"), attrs: object(rng) }).collect();
    for chunk in rows.chunks(50) {
        let t: Vec<String> = chunk.iter().map(|r| format!("({}, {})", lit(&r.id), r.attrs.as_ref().map_or("NULL".into(), |j| lit(&j.to_string())))).collect();
        sql(db, &format!("INSERT INTO t (id, attrs) VALUES {}", t.join(", ")));
        let u: Vec<String> = chunk.iter().map(|r| format!("({}, {})", lit(&r.id), r.text().map_or("NULL".into(), |s| lit(&s)))).collect();
        sql(db, &format!("INSERT INTO u (id, body) VALUES {}", u.join(", ")));
    }
    rows
}

/// MATCH finds the rows whose strings hold the words; bm25 scores them as
/// the TEXT field with the same strings does.
fn check(db: &Db, rows: &[Row]) {
    for (i, w) in WORDS.iter().enumerate() {
        let want: Vec<String> = rows.iter().filter(|r| r.has(w)).map(|r| r.id.clone()).collect();
        assert_eq!(ids(sql(db, &format!("SELECT id FROM t WHERE MATCH(attrs, '{w}')"))), want, "MATCH {w}");
        let v = WORDS[(i + 3) % WORDS.len()];
        let want: Vec<String> = rows.iter().filter(|r| r.has(w) && !r.has(v)).map(|r| r.id.clone()).collect();
        assert_eq!(ids(sql(db, &format!("SELECT id FROM t WHERE MATCH(attrs, '{w} NOT {v}')"))), want, "MATCH {w} NOT {v}");
        let scored = |table: &str, field: &str| -> Vec<(String, i64)> {
            let mut v: Vec<(String, i64)> = sql(db, &format!("SELECT id, score() FROM {table} ORDER BY bm25({field}, '{w} {v}') DESC LIMIT 5000"))
                .into_iter()
                .map(|r| (r[0].as_str().unwrap().to_string(), (r[1].as_f64().unwrap() * 1e4).round() as i64))
                .collect();
            v.sort();
            v
        };
        let (got, want) = (scored("t", "attrs"), scored("u", "body"));
        assert!(!want.is_empty() || rows.iter().all(|r| !r.has(w) && !r.has(v)));
        assert_eq!(got, want, "bm25 of {w} {v}");
    }
    // A key is not text, nor is a number.
    let keyed: Vec<String> = rows.iter().filter(|r| r.has("zebra")).map(|r| r.id.clone()).collect();
    assert_eq!(ids(sql(db, "SELECT id FROM t WHERE MATCH(attrs, 'zebra') OR MATCH(attrs, 'true')")), keyed);
}

#[test]
fn the_strings_of_a_json_field_are_searched_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let mut rng = StdRng::seed_from_u64(5);
    let mut rows = {
        let db = Db::create(&path, seed(), opts()).unwrap();
        sql(&db, "CREATE TABLE t (id KEYWORD PRIMARY KEY, attrs JSON OPTIONS(text = true))");
        sql(&db, "CREATE TABLE u (id KEYWORD PRIMARY KEY, body TEXT)");
        let rows = fill(&db, &mut rng, 0, 300);
        check(&db, &rows);
        rows
    };
    // Reopen (the memtable comes back from the WAL), flush, merge.
    let db = Db::open(&path, opts()).unwrap();
    check(&db, &rows);
    db.storage().flush().unwrap();
    check(&db, &rows);
    rows.extend(fill(&db, &mut rng, 300, 200));
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    check(&db, &rows);
    // Updates and deletes.
    sql(&db, "UPDATE t SET attrs = '{\"title\": \"kiwi heron\"}' WHERE id = 'r0001'");
    sql(&db, "UPDATE u SET body = 'kiwi heron' WHERE id = 'r0001'");
    sql(&db, "DELETE FROM t WHERE MATCH(attrs, 'otter')");
    let gone: Vec<String> = rows.iter().filter(|r| r.id != "r0001" && r.has("otter")).map(|r| r.id.clone()).collect();
    sql(&db, &format!("DELETE FROM u WHERE id IN ({})", gone.iter().map(|g| lit(g)).collect::<Vec<_>>().join(", ")));
    rows.retain(|r| !gone.contains(&r.id));
    rows.iter_mut().filter(|r| r.id == "r0001").for_each(|r| r.attrs = Some(json!({"title": "kiwi heron"})));
    check(&db, &rows);
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    check(&db, &rows);
    // A long string is text, though it is not a value to compare.
    let long = rows.iter().find(|r| r.text().is_some_and(|t| t.contains("padding"))).expect("a long string");
    assert!(ids(sql(&db, "SELECT id FROM t WHERE MATCH(attrs, 'padding')")).contains(&long.id));
    // Paths still filter beside the text.
    let want: Vec<String> = rows.iter().filter(|r| r.has("mango") && r.attrs.as_ref().is_some_and(|j| j.get("spec").is_some())).map(|r| r.id.clone()).collect();
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE MATCH(attrs, 'mango') AND attrs.spec IS NOT NULL")), want);
}

#[test]
fn text_is_an_option_of_a_json_column_that_tables_keep() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.strata");
    let db = Db::create(&path, seed(), opts()).unwrap();
    sql(&db, "CREATE TABLE p (id KEYWORD PRIMARY KEY, attrs JSON OPTIONS(text = true, analyzer = 'simple'), plain JSON)");
    let schema = db.storage().table("p").unwrap();
    let create = strata::query::ddl::create_sql(&schema);
    assert!(create.contains("attrs JSON OPTIONS(analyzer = 'simple', text = TRUE)"), "{create}");
    assert!(!create.contains("plain JSON OPTIONS"), "{create}");
    // ... which makes the same table again.
    sql(&db, &create.replacen("CREATE TABLE p", "CREATE TABLE p2", 1));
    assert_eq!(db.storage().table("p2").unwrap().fields[1].text, true);
    // The database says it needs the feature.
    let manifest = strata::storage::manifest::read(&path).unwrap().features();
    assert!(manifest.iter().any(|f| f == "json_text") && manifest.iter().any(|f| f == "json"), "{manifest:?}");
    sql(&db, r#"INSERT INTO p (id, attrs, plain) VALUES ('a', '{"t": "Running Dogs"}', '{"t": "running"}')"#);
    // The simple analyser does not stem.
    assert_eq!(ids(sql(&db, "SELECT id FROM p WHERE MATCH(attrs, 'running')")), vec!["a"]);
    assert!(sql(&db, "SELECT id FROM p WHERE MATCH(attrs, 'run')").is_empty());
    // A JSON field without text says how to get it.
    let e = db.sql("SELECT id FROM p WHERE MATCH(plain, 'running')", &Params::new()).unwrap_err().to_string();
    assert!(e.contains("OPTIONS(text = true)"), "{e}");
    let e = db.sql("SELECT id FROM p ORDER BY bm25(plain, 'running') DESC", &Params::new()).unwrap_err().to_string();
    assert!(e.contains("text"), "{e}");
    // Refused where it means nothing.
    for bad in [
        "CREATE TABLE q (id KEYWORD PRIMARY KEY, k KEYWORD OPTIONS(text = true))",
        "CREATE TABLE q (id KEYWORD PRIMARY KEY, a JSON OPTIONS(text = 'yes'))",
        "CREATE TABLE q (id KEYWORD PRIMARY KEY, a JSON OPTIONS(text = true, indexed = false))",
        "CREATE TABLE q (id KEYWORD PRIMARY KEY, a JSON OPTIONS(analyzer = 'simple'))",
    ] {
        assert!(db.sql(bad, &Params::new()).is_err(), "{bad}");
    }
    // The short form says it too, and reads back.
    let j = schema.to_short_json();
    let f = j["fields"].as_array().unwrap().iter().find(|f| f["name"] == "attrs").unwrap();
    assert_eq!(f["text"], json!(true));
    assert!(Schema::from_json(&j.to_string()).unwrap().fields[1].text);
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(ids(sql(&db, "SELECT id FROM p WHERE MATCH(attrs, 'dogs')")), vec!["a"]);
}

#[test]
fn a_json_column_with_text_added_to_a_table_with_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("a.strata"), seed(), opts()).unwrap();
    sql(&db, "CREATE TABLE t (id KEYWORD PRIMARY KEY, name KEYWORD)");
    sql(&db, "INSERT INTO t (id, name) VALUES ('a', 'x'), ('b', 'y')");
    db.storage().flush().unwrap();
    sql(&db, "ALTER TABLE t ADD COLUMN more JSON OPTIONS(text = true)");
    sql(&db, r#"INSERT INTO t (id, name, more) VALUES ('c', 'z', '{"note": "a quokka smiles"}')"#);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE MATCH(more, 'quokka')")), vec!["c"]);
    db.storage().flush().unwrap();
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE MATCH(more, 'quokka') OR name = 'x'")), vec!["a", "c"]);
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    assert_eq!(ids(sql(&db, "SELECT id FROM t ORDER BY bm25(more, 'quokka') DESC LIMIT 5")), vec!["c"]);
}
