//! The substring index (`OPTIONS(substring = true)`): `LIKE` and `ILIKE`
//! on the field return the rows a test of every stored value returns,
//! whether a segment has the index (flushed, merged, reopened), has none
//! (the memtable's) or got the field from `ALTER TABLE`; NULL values match
//! neither a pattern nor its negation; the plan survives JSON; and what the
//! option cannot be put on is refused.

use chloedb::db::{Db, SqlResult};
use chloedb::query::ir::Plan;
use chloedb::query::Params;
use chloedb::schema::{Field, FieldType, Schema};
use chloedb::segment::build::BuildOptions;
use chloedb::storage::{DbOptions, MergePolicy};
use chloedb::value::{Document, Value};

const LOGS: &str = "CREATE TABLE logs (
    id KEYWORD PRIMARY KEY,
    line TEXT OPTIONS(substring = true, indexed = false),
    host KEYWORD OPTIONS(substring = true),
    n INT,
    m INT
)";

/// Small parts and dense samples, so that a few thousand lines make
/// several parts and every sample path is taken.
fn opts() -> DbOptions {
    DbOptions { sync: false, build: BuildOptions::default().set("substring.part_bytes", "30000").set("substring.sample", "5"), ..DbOptions::default() }
}

fn db(path: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, opts()).unwrap();
    run(&db, LOGS);
    db
}

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn fails(db: &Db, sql: &str) -> String {
    db.sql(sql, &Params::new()).err().unwrap_or_else(|| panic!("{sql}: no error")).to_string()
}

const WORDS: [&str; 16] = ["GET", "POST", "/api/v1/users", "/API/v2/Orders", "timeout", "Timeout", "ERROR", "error:", "été", "ÉTÉ", "東京都", "İstanbul", "ΟΔΟΣ", "οδός", "50%_off", "a_b"];
const HOSTS: [&str; 6] = ["web-01.eu", "web-02.eu", "WEB-03.us", "db-01.eu", "db_02.us", "cache%1"];

/// Row `i`: a line of three to seven words (none for one row in eleven,
/// the empty line for one in thirteen), a host (none for one in seven).
fn doc(i: usize) -> Document {
    let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x2545_F491;
    let mut next = |m: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % m as u64) as usize
    };
    let line = if i % 11 == 3 {
        Value::Null
    } else if i % 13 == 5 {
        Value::Str(String::new())
    } else {
        let words: Vec<&str> = (0..3 + next(5)).map(|_| WORDS[next(WORDS.len())]).collect();
        Value::Str(format!("{} req={}", words.join(" "), next(1000)))
    };
    let host = if i % 7 == 2 { Value::Null } else { Value::Str(HOSTS[next(HOSTS.len())].to_string()) };
    vec![Value::Str(format!("r{i:05}")), line, host, Value::Int(i as i64), Value::Int((i % 10) as i64)]
}

/// Whether `text` matches LIKE `pattern`: `%` any run of characters, `_`
/// one.
fn like(text: &[char], pattern: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('%', rest)) => (0..=text.len()).any(|i| like(&text[i..], rest)),
        Some(('_', rest)) => !text.is_empty() && like(&text[1..], rest),
        Some((c, rest)) => text.first() == Some(c) && like(&text[1..], rest),
    }
}

/// `value [NOT] [I]LIKE pattern`, UNKNOWN (false) for NULL.
fn holds(value: &Value, pattern: &str, ci: bool, negated: bool) -> bool {
    let Some(v) = value.as_str() else { return false };
    let (t, p): (Vec<char>, Vec<char>) = if ci { (fold(v).chars().collect(), fold(pattern).chars().collect()) } else { (v.chars().collect(), pattern.chars().collect()) };
    like(&t, &p) != negated
}

/// `s` as ILIKE compares it, as PostgreSQL does: each character lowercased
/// alone to one character, and a final sigma as the other sigma.
fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c.to_lowercase().next().unwrap() {
            'ς' => 'σ',
            c => c,
        })
        .collect()
}

fn ids(db: &Db, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")).rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    out.sort();
    out
}

const PATTERNS: [&str; 34] = [
    "%timeout%", "%Timeout%", "GET%", "%req=7", "%req=7%", "%/api/%users%", "%", "", "%%", "_%", "%_", "GET POST%", "%error:%req=1__", "%ERROR%ERROR%", "%été%", "%ÉTÉ%",
    "%東京%", "%京_ %", "%İstanbul%", "%istanbul%", "%i̇stanbul%", "%ΟΔΟΣ%", "%οδός%", "%οδόσ%", "%50%_off%", "%a_b%", "%nothing here%", "%é%", "_ET%", "%=__", "%ø%", "%\u{FF}%", "%S%", "%s%",
];

const HOST_PATTERNS: [&str; 12] = ["web%", "%eu", "%-0_.%", "web-01.eu", "WEB%", "%b%", "db_02.us", "db__2%", "%", "", "cache%1", "%.US"];

/// Every pattern, as LIKE, NOT LIKE, ILIKE and NOT ILIKE, alone and with
/// other conditions, returns the rows a test of `docs` returns.
fn check(db: &Db, docs: &[Document], at: &str) {
    let want = |f: &dyn Fn(&Document) -> bool| -> Vec<String> {
        let mut out: Vec<String> = docs.iter().filter(|d| f(d)).map(|d| d[0].as_str().unwrap().to_string()).collect();
        out.sort();
        out
    };
    for (field, col, patterns) in [("line", 1, &PATTERNS[..]), ("host", 2, &HOST_PATTERNS[..])] {
        for pattern in patterns {
            let quoted = pattern.replace('\'', "''");
            for (op, ci, negated) in [("LIKE", false, false), ("NOT LIKE", false, true), ("ILIKE", true, false), ("NOT ILIKE", true, true)] {
                let sql = format!("SELECT id FROM logs WHERE {field} {op} '{quoted}' LIMIT 100000");
                assert_eq!(ids(db, &sql), want(&|d| holds(&d[col], pattern, ci, negated)), "{at}: {sql}");
            }
            let sql = format!("SELECT id FROM logs WHERE {field} LIKE '{quoted}' AND n % 3 = 0 AND n < 2500 LIMIT 100000");
            assert_eq!(ids(db, &sql), want(&|d| holds(&d[col], pattern, false, false) && d[3].as_i64().unwrap() % 3 == 0 && d[3].as_i64().unwrap() < 2500), "{at}: {sql}");
            let sql = format!("SELECT id FROM logs WHERE NOT ({field} ILIKE '{quoted}' OR n >= 1000) LIMIT 100000");
            assert_eq!(ids(db, &sql), want(&|d| holds(&d[col], pattern, true, true) && d[3].as_i64().unwrap() < 1000), "{at}: {sql}");
            // A few rows by another condition: the pattern is tested on
            // their values where that costs less than the whole segment's.
            for (op, ci, negated) in [("LIKE", false, false), ("NOT LIKE", false, true), ("ILIKE", true, false), ("NOT ILIKE", true, true)] {
                let sql = format!("SELECT id FROM logs WHERE n BETWEEN 40 AND 47 AND {field} {op} '{quoted}' LIMIT 100000");
                assert_eq!(ids(db, &sql), want(&|d| holds(&d[col], pattern, ci, negated) && (40..=47).contains(&d[3].as_i64().unwrap())), "{at}: {sql}");
                // The two halves of a range, each of which keeps most
                // rows, and the rows in an order.
                let sql = format!("SELECT id FROM logs WHERE n >= 40 AND n < 48 AND {field} {op} '{quoted}' ORDER BY n DESC LIMIT 100000");
                assert_eq!(ids(db, &sql), want(&|d| holds(&d[col], pattern, ci, negated) && (40..=47).contains(&d[3].as_i64().unwrap())), "{at}: {sql}");
            }
            let sql = format!("SELECT count(*) FROM logs WHERE {field} LIKE '{quoted}'");
            let n = db.query(&sql, &Params::new()).unwrap().rows[0][0].as_i64().unwrap();
            assert_eq!(n as usize, want(&|d| holds(&d[col], pattern, false, false)).len(), "{at}: {sql}");
        }
    }
    // Both fields at once, and the pattern of one against the other's.
    let sql = "SELECT id FROM logs WHERE line LIKE '%timeout%' AND host ILIKE 'web%' OR line IS NULL AND host NOT LIKE '%eu' LIMIT 100000";
    assert_eq!(
        ids(db, sql),
        want(&|d| holds(&d[1], "%timeout%", false, false) && holds(&d[2], "web%", true, false) || d[1] == Value::Null && holds(&d[2], "%eu", false, true)),
        "{at}: {sql}"
    );
}

/// The segments of `logs` with a substring index of `field`, and their
/// parts in all.
fn indexed(db: &Db, field: &str) -> (usize, usize) {
    let snap = db.snapshot().table("logs").unwrap();
    let f = db.catalog().iter().find(|s| s.table == "logs").unwrap().field_index(field).unwrap();
    let mut out = (0, 0);
    for r in db.readers(&snap).unwrap() {
        if let Some(ix) = r.substring(f).unwrap() {
            out.0 += 1;
            out.1 += ix.parts().len();
        }
    }
    out
}

#[test]
fn like_and_ilike_through_the_lifecycle() {
    let docs: Vec<Document> = (0..5000).map(doc).collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.strata");
    {
        let db = db(&path);
        // The memtable alone: no index, every value tested.
        db.load_into("logs", docs[..1500].iter().cloned()).unwrap();
        check(&db, &docs[..1500], "memtable");
        db.storage().flush().unwrap();
        let (segments, parts) = indexed(&db, "line");
        assert!(segments >= 1 && parts > segments, "{segments} segments with {parts} parts");
        assert!(indexed(&db, "host").0 >= 1);
        check(&db, &docs[..1500], "flushed");
        // A second segment, and rows still in the memtable.
        db.load_into("logs", docs[1500..3500].iter().cloned()).unwrap();
        db.storage().flush().unwrap();
        db.load_into("logs", docs[3500..].iter().cloned()).unwrap();
        check(&db, &docs, "two segments and a memtable");
    }
    let db = Db::open(&path, opts()).unwrap();
    check(&db, &docs, "reopened");
    db.storage().flush().unwrap();
    // Rows deleted and rewritten leave their old values in the index, out
    // of the answer.
    let mut docs = docs;
    run(&db, "DELETE FROM logs WHERE m = 4");
    run(&db, "UPDATE logs SET line = 'rewritten Timeout LINE' WHERE m = 6");
    run(&db, "UPDATE logs SET host = NULL WHERE m = 8");
    // The index finds the rows of a write too.
    run(&db, "DELETE FROM logs WHERE line LIKE '%req=99_'");
    run(&db, "UPDATE logs SET host = 'moved' WHERE host ILIKE 'DB%' AND line NOT LIKE '%GET%'");
    docs.retain(|d| d[4] != Value::Int(4));
    for d in &mut docs {
        match d[4].as_i64().unwrap() {
            6 => d[1] = Value::Str("rewritten Timeout LINE".into()),
            8 => d[2] = Value::Null,
            _ => {}
        }
    }
    docs.retain(|d| !holds(&d[1], "%req=99_", false, false));
    for d in &mut docs {
        if holds(&d[2], "db%", true, false) && holds(&d[1], "%GET%", false, true) {
            d[2] = Value::Str("moved".into());
        }
    }
    check(&db, &docs, "deleted and updated");
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 100_000, ..MergePolicy::default() }).unwrap();
    assert!(indexed(&db, "line").0 >= 1);
    check(&db, &docs, "merged");
    db.optimize().unwrap();
    assert_eq!(indexed(&db, "line").0, 1);
    check(&db, &docs, "one segment");
}

/// A field added to a table with rows: the old segments answer for its
/// fill value, the new ones for what was written.
#[test]
fn a_field_added_later() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("a.strata"));
    db.load_into("logs", (0..300).map(doc)).unwrap();
    db.storage().flush().unwrap();
    run(&db, "ALTER TABLE logs ADD COLUMN note TEXT OPTIONS(substring = true, indexed = false) DEFAULT 'none yet', ADD COLUMN tag KEYWORD OPTIONS(substring = true)");
    assert_eq!(ids(&db, "SELECT id FROM logs WHERE note LIKE '%ne y%' LIMIT 1000").len(), 300);
    assert_eq!(ids(&db, "SELECT id FROM logs WHERE note NOT ILIKE '%NE Y%' LIMIT 1000").len(), 0);
    assert_eq!(ids(&db, "SELECT id FROM logs WHERE tag LIKE '%' OR tag NOT LIKE '%' LIMIT 1000").len(), 0);
    run(&db, "INSERT INTO logs (id, line, note, tag) VALUES ('x1', 'a line', 'checked by Ana', 'blue-green'), ('x2', NULL, NULL, 'green')");
    for flushed in [false, true] {
        if flushed {
            db.storage().flush().unwrap();
            db.optimize().unwrap();
        }
        assert_eq!(ids(&db, "SELECT id FROM logs WHERE note ILIKE '%ana' LIMIT 1000"), ["x1"], "flushed: {flushed}");
        assert_eq!(ids(&db, "SELECT id FROM logs WHERE note LIKE '%e%' LIMIT 1000").len(), 301, "flushed: {flushed}");
        assert_eq!(ids(&db, "SELECT id FROM logs WHERE tag LIKE '%green' LIMIT 1000"), ["x1", "x2"], "flushed: {flushed}");
        assert_eq!(ids(&db, "SELECT id FROM logs WHERE tag LIKE '_____green' LIMIT 1000"), ["x1"], "flushed: {flushed}");
        assert_eq!(ids(&db, "SELECT id FROM logs WHERE tag NOT LIKE 'blue%' LIMIT 1000"), ["x2"], "flushed: {flushed}");
    }
}

/// The plan of a LIKE on the field is the row evaluator's `LIKE`, which a
/// reader of the plan's JSON runs to the same rows.
#[test]
fn the_plan_survives_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("p.strata"));
    let docs: Vec<Document> = (0..800).map(doc).collect();
    db.load_into("logs", docs[..600].iter().cloned()).unwrap();
    db.storage().flush().unwrap();
    db.load_into("logs", docs[600..].iter().cloned()).unwrap();
    for sql in [
        "SELECT id FROM logs WHERE line LIKE '%api%users%' ORDER BY id LIMIT 10000",
        "SELECT id FROM logs WHERE line NOT ILIKE '%TIMEOUT%' AND n > 10 ORDER BY id LIMIT 10000",
        "SELECT id FROM logs WHERE host ILIKE 'web%' OR NOT (line LIKE '%_' AND host LIKE '%eu') ORDER BY id LIMIT 10000",
        "SELECT host, count(*) FROM logs WHERE line LIKE '%ERROR%' GROUP BY host ORDER BY host",
    ] {
        let plan = db.explain(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let json = plan.to_json().unwrap();
        let back = Plan::from_json(&json).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(back, plan, "{sql}");
        let (SqlResult::Rows(a), SqlResult::Rows(b)) = (run(&db, sql), db.run(&back).unwrap_or_else(|e| panic!("{sql}: {e}"))) else { panic!("{sql}") };
        assert!(!a.rows.is_empty(), "{sql}");
        assert_eq!(a.rows, b.rows, "{sql}");
    }
}

#[test]
fn the_option_is_declared_and_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"));
    let schema = db.catalog().into_iter().find(|s| s.table == "logs").unwrap();
    assert!(schema.field("line").unwrap().substring && schema.field("host").unwrap().substring && !schema.field("id").unwrap().substring);
    assert!(schema.features().any(|f| f == "substring:1"));
    // The schema's JSON carries it.
    let json = serde_json::to_string(&*schema).unwrap();
    let back: Schema = serde_json::from_str(&json).unwrap();
    assert!(back.field("line").unwrap().substring);
    for (sql, why) in [
        ("CREATE TABLE t (id KEYWORD PRIMARY KEY, n INT OPTIONS(substring = true))", "substring"),
        ("CREATE TABLE t (id KEYWORD PRIMARY KEY, tags KEYWORD[] OPTIONS(substring = true))", "not an option"),
        ("CREATE TABLE t (id KEYWORD PRIMARY KEY, body TEXT OPTIONS(substring = true, stored = false))", "stored"),
        ("CREATE TABLE t (id KEYWORD PRIMARY KEY, body TEXT OPTIONS(substring = 'yes'))", "true or false"),
        ("SELECT id FROM logs WHERE line LIKE '%50!%%' ESCAPE '!'", "ESCAPE"),
        ("SELECT id FROM logs WHERE line LIKE 7", "text"),
    ] {
        let e = fails(&db, sql);
        assert!(e.contains(why), "{sql}: {e}");
    }
    // A KEYWORD field's own LIKE still takes ESCAPE for a prefix.
    db.load_into("logs", (0..50).map(doc)).unwrap();
    assert_eq!(ids(&db, "SELECT id FROM logs WHERE host LIKE 'cache!%%' ESCAPE '!'"), ids(&db, "SELECT id FROM logs WHERE host LIKE 'cache_1'"));
}

/// A NULL pattern matches no value and neither does its negation, as in
/// SQL (finding N-B10): on a field with the index as on one without, for
/// LIKE, NOT LIKE, ILIKE and NOT ILIKE, written out or as a parameter. It
/// was an error on a field with the index ("LIKE's pattern must be text").
#[test]
fn a_null_pattern_matches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("n.strata"));
    run(&db, "CREATE TABLE plain (id KEYWORD PRIMARY KEY, line TEXT, host KEYWORD)");
    let docs: Vec<Document> = (0..300).map(doc).collect();
    db.load_into("logs", docs.iter().cloned()).unwrap();
    db.storage().flush().unwrap();
    db.load_into("logs", (300..400).map(doc)).unwrap();
    run(&db, "INSERT INTO plain VALUES ('a', 'GET /api', 'web-01.eu'), ('b', NULL, NULL)");
    let null = Params::new().set("p", Value::Null);
    for table in ["logs", "plain"] {
        for field in ["line", "host"] {
            for op in ["LIKE", "NOT LIKE", "ILIKE", "NOT ILIKE"] {
                if op.contains("ILIKE") && table == "plain" && field == "host" {
                    continue; // a KEYWORD field's index takes no ILIKE
                }
                for (sql, params) in [(format!("SELECT id FROM {table} WHERE {field} {op} NULL"), Params::new()), (format!("SELECT id FROM {table} WHERE {field} {op} :p"), null.clone())] {
                    let rs = db.query(&sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
                    assert!(rs.rows.is_empty(), "{sql}: {:?}", rs.rows);
                    let sql = format!("SELECT id FROM {table} WHERE NOT ({field} {op} :p)");
                    assert!(db.query(&sql, &null).unwrap_or_else(|e| panic!("{sql}: {e}")).rows.is_empty(), "{sql}");
                }
            }
        }
    }
}
