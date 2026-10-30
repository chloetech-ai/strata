//! BOOL and TIMESTAMP fields, the first features of format 3: written from
//! SQL, the Rust API, JSON and CSV; filtered, sorted, grouped, aggregated
//! and decayed; answered as booleans and times, also by shards merged by
//! `cluster::merge`; and the file format they need, in both layouts: 3
//! while a table uses one and back to the older format when none does, and
//! refused by name by a reader that does not know a feature.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use strata::cluster;
use strata::db::{Db, SqlResult};
use strata::ingest::{self, Format, Rows};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::schema::{Field, FieldType, Schema};
use strata::storage::pager::{Meta, PAGE};
use strata::storage::{DbOptions, Layout};
use strata::value::Value;

/// 2026-10-01T00:00:00Z in microseconds.
const OCT1: i64 = 1_790_812_800_000_000;
const HOUR: i64 = 3_600_000_000;
const DAY: i64 = 24 * HOUR;

fn papers() -> Schema {
    Schema::new("papers", vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Text { positions: true })], Some("id")).unwrap()
}

fn tasks() -> Schema {
    let fields = vec![Field::new("id", FieldType::Keyword), Field::boolean("done"), Field::timestamp("at"), Field::new("n", FieldType::Int)];
    Schema::new("tasks", fields, Some("id")).unwrap()
}

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn written(db: &Db, sql: &str) -> u64 {
    match run(db, sql) {
        SqlResult::Written(n) => n,
        SqlResult::Rows(_) => panic!("{sql}: expected a write"),
    }
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => panic!("{sql}: expected rows"),
    }
}

/// The rows sorted by their first column, a KEYWORD.
fn by_id(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    let mut r = rows(db, sql);
    r.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
    r
}

/// The first column of each row, as text.
fn ids(db: &Db, sql: &str) -> Vec<String> {
    rows(db, sql).into_iter().map(|r| r[0].as_str().unwrap_or_else(|| panic!("{sql}: {r:?}")).to_string()).collect()
}

fn fails(db: &Db, sql: &str) -> String {
    match db.sql(sql, &Params::new()) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(e) => e.to_string(),
    }
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn t(micros: i64) -> Value {
    Value::Timestamp(micros)
}

/// The format of the version on disk: CURRENT's, or the newest meta slot's.
fn format_on_disk(path: &Path) -> u32 {
    if path.is_dir() {
        let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
        return cur.get("format").map_or(1, |f| f.as_u64().unwrap() as u32);
    }
    meta_slots(path).into_iter().flatten().max_by_key(|m| m.txn).unwrap().format
}

fn meta_slots(path: &Path) -> Vec<Option<Meta>> {
    let f = File::open(path).unwrap();
    (0..2u64)
        .map(|slot| {
            let mut page = vec![0u8; PAGE as usize];
            f.read_exact_at(&mut page, slot * PAGE).unwrap();
            Meta::decode(&page).unwrap()
        })
        .collect()
}

#[test]
fn sql_single_file() {
    through_sql(Layout::SingleFile);
}

#[test]
fn sql_directory() {
    through_sql(Layout::Directory);
}

fn through_sql(layout: Layout) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let opts = || DbOptions { layout, sync: false, ..DbOptions::default() };
    let db = Db::create(&path, papers(), opts()).unwrap();
    assert_eq!(format_on_disk(&path), 1);

    // Neither type can be a key, and DATE points to TIMESTAMP.
    assert!(fails(&db, "CREATE TABLE z (id BOOL PRIMARY KEY)").contains("primary key"));
    assert!(fails(&db, "CREATE TABLE z (id KEYWORD PRIMARY KEY, d DATE)").contains("TIMESTAMP"));
    assert_eq!(format_on_disk(&path), 1);

    written(&db, "CREATE TABLE tasks (id KEYWORD PRIMARY KEY, title TEXT, done BOOL, due TIMESTAMP, n INT)");
    assert_eq!(format_on_disk(&path), 3);
    let n = written(
        &db,
        "INSERT INTO tasks (id, title, done, due, n) VALUES \
         ('a', 'write the parser', TRUE, '2026-10-01T09:00:00Z', 1), \
         ('b', 'review it', FALSE, '2026-10-02', 2), \
         ('c', 'ship it', NULL, '2026-09-30T23:30:00-02:00', 3), \
         ('d', 'plan the next', 't', NULL, 4)",
    );
    assert_eq!(n, 4);

    // Wrong values are refused, and say what the field takes.
    let e = fails(&db, "INSERT INTO tasks (id, due) VALUES ('x', 1790812800)");
    assert!(e.contains("TIMESTAMP") && e.contains("'2026-10-01T12:30:00Z'"), "{e}");
    assert!(fails(&db, "INSERT INTO tasks (id, due) VALUES ('x', '2026-02-30')").contains("due"));
    assert!(fails(&db, "INSERT INTO tasks (id, done) VALUES ('x', 2)").contains("done"));
    assert!(fails(&db, "INSERT INTO tasks (id, done) VALUES ('x', 'maybe')").contains("done"));
    assert!(fails(&db, "SELECT id FROM tasks WHERE due > 'tomorrow'").contains("due"));
    assert!(fails(&db, "SELECT id FROM tasks WHERE done = 2").contains("done"));
    assert!(fails(&db, "SELECT SUM(due) FROM tasks").contains("TIMESTAMP"));
    assert!(fails(&db, "SELECT AVG(due) FROM tasks").contains("TIMESTAMP"));
    assert!(fails(&db, "SELECT id FROM tasks WHERE n IS TRUE").contains("BOOL"));
    assert_eq!(ids(&db, "SELECT id FROM tasks ORDER BY n"), ["a", "b", "c", "d"]);

    let check = |db: &Db| {
        // Values come back as booleans and times, NULL as NULL.
        assert_eq!(
            rows(db, "SELECT id, done, due FROM tasks ORDER BY n"),
            [
                vec![s("a"), Value::Bool(true), t(OCT1 + 9 * HOUR)],
                vec![s("b"), Value::Bool(false), t(OCT1 + DAY)],
                vec![s("c"), Value::Null, t(OCT1 + 3 * HOUR / 2)],
                vec![s("d"), Value::Bool(true), Value::Null],
            ]
        );
        let q = |w: &str| ids(db, &format!("SELECT id FROM tasks WHERE {w} ORDER BY n"));
        assert_eq!(q("done"), ["a", "d"]);
        assert_eq!(q("done = TRUE"), ["a", "d"]);
        assert_eq!(q("done = 1"), ["a", "d"]);
        // NULL is neither true nor false: NOT and != leave it out, IS NOT
        // TRUE and IS NOT FALSE take it in.
        assert_eq!(q("NOT done"), ["b"]);
        assert_eq!(q("done != TRUE"), ["b"]);
        assert_eq!(q("done = FALSE"), ["b"]);
        assert_eq!(q("done IS TRUE"), ["a", "d"]);
        assert_eq!(q("done IS FALSE"), ["b"]);
        assert_eq!(q("done IS NOT TRUE"), ["b", "c"]);
        assert_eq!(q("done IS NOT FALSE"), ["a", "c", "d"]);
        assert_eq!(q("done IS NULL"), ["c"]);
        assert_eq!(q("done AND n > 1"), ["d"]);
        assert_eq!(q("done OR due < '2026-10-01T05:00:00Z'"), ["a", "c", "d"]);
        // Times compare as times: c is 01:30 UTC on October 1.
        assert_eq!(q("due >= '2026-10-01'"), ["a", "b", "c"]);
        assert_eq!(q("due < '2026-10-01T05:00:00Z'"), ["c"]);
        assert_eq!(q("due < '2026-10-01T05:00:00+02:00'"), ["c"]);
        assert_eq!(q("due > '2026-10-01 01:30'"), ["a", "b"]);
        assert_eq!(q("due BETWEEN '2026-10-01' AND '2026-10-01T12:00:00Z'"), ["a", "c"]);
        assert_eq!(q("due NOT BETWEEN '2026-10-01' AND '2026-10-01T12:00:00Z'"), ["b"]);
        assert_eq!(q("due IN ('2026-10-02', '2026-10-01T03:30:00+02:00')"), ["b", "c"]);
        assert_eq!(q("due = '2026-10-02T00:00:00.000Z'"), ["b"]);
        assert_eq!(q("due IS NULL"), ["d"]);
        assert_eq!(ids(db, "SELECT id FROM tasks WHERE due IS NOT NULL ORDER BY due DESC"), ["b", "a", "c"]);
        assert_eq!(ids(db, "SELECT id FROM tasks WHERE due IS NOT NULL ORDER BY due"), ["c", "a", "b"]);

        // Aggregates: MIN and MAX are times, SUM counts the trues.
        assert_eq!(rows(db, "SELECT MIN(due), MAX(due), COUNT(due), SUM(done) FROM tasks"), [vec![t(OCT1 + 3 * HOUR / 2), t(OCT1 + DAY), Value::Int(3), Value::Int(2)]]);
        let mut groups = rows(db, "SELECT done, COUNT(*), MAX(due) FROM tasks GROUP BY done");
        groups.sort_by_key(|r| format!("{:?}", r[0]));
        assert_eq!(
            groups,
            [
                vec![Value::Bool(false), Value::Int(1), t(OCT1 + DAY)],
                vec![Value::Bool(true), Value::Int(2), t(OCT1 + 9 * HOUR)],
                vec![Value::Null, Value::Int(1), t(OCT1 + 3 * HOUR / 2)],
            ]
        );
        let mut late = rows(db, "SELECT done, MAX(due) FROM tasks GROUP BY done HAVING MAX(due) > '2026-10-01T06:00:00Z'");
        late.sort_by_key(|r| format!("{:?}", r[0]));
        assert_eq!(late, [vec![Value::Bool(false), t(OCT1 + DAY)], vec![Value::Bool(true), t(OCT1 + 9 * HOUR)]]);

        // Compound queries carry the types through.
        let mut distinct = rows(db, "SELECT DISTINCT done FROM tasks");
        distinct.sort_by_key(|r| format!("{:?}", r[0]));
        assert_eq!(distinct, [vec![Value::Bool(false)], vec![Value::Bool(true)], vec![Value::Null]]);
        assert_eq!(ids(db, "SELECT id FROM tasks WHERE due IN (SELECT due FROM tasks WHERE done) ORDER BY n"), ["a"]);
        assert_eq!(
            rows(db, "WITH late AS (SELECT id, due FROM tasks WHERE due >= '2026-10-01T06:00:00Z') SELECT id, due FROM late ORDER BY due DESC"),
            [vec![s("b"), t(OCT1 + DAY)], vec![s("a"), t(OCT1 + 9 * HOUR)]]
        );
        assert_eq!(
            rows(db, "SELECT x.id, x.done FROM (SELECT id, done, due, n FROM tasks) x WHERE x.due < '2026-10-01T12:00:00Z' ORDER BY x.n"),
            [vec![s("a"), Value::Bool(true)], vec![s("c"), Value::Null]]
        );
    };
    check(&db);
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(format_on_disk(&path), 3);
    check(&db);

    // Updates set the types' values, and refuse arithmetic on them.
    assert_eq!(written(&db, "UPDATE tasks SET done = TRUE, due = '2026-10-03T00:00:00+00:00' WHERE id = 'b'"), 1);
    assert_eq!(ids(&db, "SELECT id FROM tasks WHERE done ORDER BY n"), ["a", "b", "d"]);
    assert_eq!(rows(&db, "SELECT due FROM tasks WHERE id = 'b'"), [vec![t(OCT1 + 2 * DAY)]]);
    assert!(fails(&db, "UPDATE tasks SET done = done + 1 WHERE id = 'a'").contains("done"));
    assert!(fails(&db, "UPDATE tasks SET due = due + 1 WHERE id = 'a'").contains("due"));
    written(&db, "INSERT INTO tasks (id, done, due) VALUES ('c', FALSE, '2026-11-01') ON CONFLICT (id) DO UPDATE SET done = excluded.done, due = excluded.due");
    assert_eq!(rows(&db, "SELECT done, due FROM tasks WHERE id = 'c'"), [vec![Value::Bool(false), t(OCT1 + 31 * DAY)]]);
    assert!(fails(&db, "INSERT INTO tasks (id, n) VALUES ('c', 1) ON CONFLICT (id) DO UPDATE SET done = excluded.n").contains("done"));

    // A database whose tables use no feature is written in the older
    // format again, in both meta slots of a single file.
    written(&db, "DROP TABLE tasks");
    assert_eq!(format_on_disk(&path), 1);
    if layout == Layout::SingleFile {
        let slots = meta_slots(&path);
        assert!(slots.iter().all(|m| m.is_some_and(|m| m.format == 1)), "{slots:?}");
    }
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(db.catalog().len(), 1);
    assert_eq!(format_on_disk(&path), 1);
}

#[test]
fn rust_api_json_and_csv() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.strata");
    let db = Db::create(&path, tasks(), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    // The table the database is created with already needs format 3.
    assert_eq!(format_on_disk(&path), 3);

    db.load([
        vec![s("a"), Value::Bool(true), t(OCT1), Value::Bool(true)],
        vec![s("b"), Value::Int(0), s("2026-10-02"), Value::Int(5)],
        vec![s("c"), s("f"), Value::Int(OCT1 + HOUR), Value::Null],
    ])
    .unwrap();
    for (bad, what) in [
        (vec![s("x"), Value::Int(2), Value::Null, Value::Null], "done"),
        (vec![s("x"), s("yes"), Value::Null, Value::Null], "done"),
        (vec![s("x"), Value::Null, s("soon"), Value::Null], "at"),
        (vec![s("x"), Value::Null, Value::Float(1.5), Value::Null], "at"),
        (vec![s("x"), Value::Timestamp(OCT1), Value::Null, Value::Null], "done"),
    ] {
        let e = db.storage().begin().unwrap().insert(bad.clone()).unwrap_err().to_string();
        assert!(e.contains(what), "{bad:?}: {e}");
    }
    // A boolean given to an INT field is 1 or 0.
    assert_eq!(by_id(&db, "SELECT id, done, at, n FROM tasks"), [
        vec![s("a"), Value::Bool(true), t(OCT1), Value::Int(1)],
        vec![s("b"), Value::Bool(false), t(OCT1 + DAY), Value::Int(5)],
        vec![s("c"), Value::Bool(false), t(OCT1 + HOUR), Value::Null],
    ]);

    // Values set in place take the types' values; adding to them is refused.
    let mut txn = db.storage().begin().unwrap();
    assert!(txn.set(&s("b"), "done", Value::Bool(true)).unwrap());
    assert!(txn.set(&s("b"), "at", s("2026-10-05T00:00:00Z")).unwrap());
    assert!(txn.set(&s("c"), "done", Value::Int(1)).unwrap());
    assert!(txn.set(&s("c"), "done", Value::Int(7)).is_err());
    assert!(txn.set(&s("c"), "at", s("not a time")).is_err());
    let e = txn.add(&s("a"), "at", &Value::Int(1)).unwrap_err().to_string();
    assert!(e.contains("set it rather than add to it"), "{e}");
    assert!(txn.add(&s("a"), "done", &Value::Int(1)).is_err());
    txn.commit().unwrap();
    assert_eq!(by_id(&db, "SELECT id, done, at FROM tasks WHERE id IN ('b', 'c')"), [
        vec![s("b"), Value::Bool(true), t(OCT1 + 4 * DAY)],
        vec![s("c"), Value::Bool(true), t(OCT1 + HOUR)],
    ]);

    // JSON: true, false, 1, 0 or their text for a BOOL; text for a TIMESTAMP.
    let schema = db.schema().clone();
    let jsonl = "{\"id\":\"d\",\"done\":true,\"at\":\"2026-10-01T10:00:00+02:00\"}\n{\"id\":\"e\",\"done\":0,\"at\":null}\n{\"id\":\"f\",\"done\":\"false\",\"n\":true}\n";
    let out = ingest::import(&db, Rows::new(&schema, std::io::Cursor::new(jsonl), Format::Jsonl).unwrap()).unwrap();
    assert_eq!(out.rows, 3);
    for bad in ["{\"id\":\"x\",\"at\":1790812800}", "{\"id\":\"x\",\"done\":3}", "{\"id\":\"x\",\"done\":[true]}", "{\"id\":\"x\",\"at\":\"2026-13-01\"}"] {
        let mut r = Rows::new(&schema, std::io::Cursor::new(bad), Format::Jsonl).unwrap();
        assert!(r.next().unwrap().is_err(), "{bad}");
    }
    // CSV: the same as text.
    let csv = "id,done,at\ng,t,2026-10-01\nh,0,\ni,,2026-10-01 12:00:00.5\n";
    ingest::import(&db, Rows::new(&schema, std::io::Cursor::new(csv), Format::Csv).unwrap()).unwrap();
    let mut r = Rows::new(&schema, std::io::Cursor::new("id,done\nx,yes\n"), Format::Csv).unwrap();
    assert!(r.next().unwrap().is_err());
    assert_eq!(by_id(&db, "SELECT id, done, at, n FROM tasks WHERE id IN ('d', 'e', 'f', 'g', 'h', 'i')"), [
        vec![s("d"), Value::Bool(true), t(OCT1 + 8 * HOUR), Value::Null],
        vec![s("e"), Value::Bool(false), Value::Null, Value::Null],
        vec![s("f"), Value::Bool(false), Value::Null, Value::Int(1)],
        vec![s("g"), Value::Bool(true), t(OCT1), Value::Null],
        vec![s("h"), Value::Bool(false), Value::Null, Value::Null],
        vec![s("i"), Value::Null, t(OCT1 + 12 * HOUR + 500_000), Value::Null],
    ]);

    // Documents read back from the store hold the stored integers.
    let snap = db.snapshot();
    let a = db.documents(&snap).map(Result::unwrap).find(|d| d[0] == s("a")).unwrap();
    assert_eq!(a[1..3], [Value::Int(1), Value::Int(OCT1)]);

    // Decay on a TIMESTAMP: the origin is a time, the scale seconds.
    let r = db.query("SELECT id, score() FROM tasks WHERE at IS NOT NULL ORDER BY decay(at, '2026-10-01', 86400) DESC LIMIT 3", &Params::new()).unwrap();
    assert_eq!(r.keys, [s("a"), s("g"), s("c")]);
    assert_eq!(r.scores[0], 1.0);
    let r = db.query("SELECT id FROM tasks WHERE id = 'b' ORDER BY decay(at, '2026-10-04', 86400) DESC", &Params::new()).unwrap();
    assert!((r.scores[0] - 0.5).abs() < 1e-9, "{:?}", r.scores);
    let r = db.query("SELECT id FROM tasks ORDER BY decay(at, now(), 86400) DESC LIMIT 1", &Params::new()).unwrap();
    assert_eq!(r.keys.len(), 1);
    assert!(db.query("SELECT id FROM tasks ORDER BY decay(at, 1790812800, 86400) DESC", &Params::new()).is_err());
}

/// The coordinator's two rounds over `shards`, as in `cluster_split.rs`.
fn sharded(shards: &[&Db], sql: &str) -> ResultSet {
    let params = Params::new();
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let select = bind(sql, snaps[0].schema(), &params).unwrap();
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
    let stats = Arc::new(TermStats::sum(&parts));
    let opts = ExecOptions { stats: Some(stats), shard: true, ..ExecOptions::default() };
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap()).collect();
    cluster::merge(&select, answers).unwrap()
}

#[test]
fn shards_answer_in_the_types() {
    let dir = tempfile::tempdir().unwrap();
    let opts = || DbOptions { sync: false, segment_max_docs: 50, ..DbOptions::default() };
    let docs: Vec<_> = (0..300i64)
        .map(|i| {
            let done = match i % 3 {
                0 => Value::Bool(true),
                1 => Value::Bool(false),
                _ => Value::Null,
            };
            let at = if i % 7 == 0 { Value::Null } else { t(OCT1 + (i * 37 % 300 - 150) * HOUR) };
            vec![s(&format!("k{i:03}")), done, at, Value::Int(i)]
        })
        .collect();
    let whole = Db::create(&dir.path().join("whole.strata"), tasks(), opts()).unwrap();
    whole.load(docs.iter().cloned()).unwrap();
    let a = Db::create(&dir.path().join("a.strata"), tasks(), opts()).unwrap();
    let b = Db::create(&dir.path().join("b.strata"), tasks(), opts()).unwrap();
    a.load(docs.iter().step_by(2).cloned()).unwrap();
    b.load(docs.iter().skip(1).step_by(2).cloned()).unwrap();

    for sql in [
        "SELECT done, COUNT(*), MIN(at), MAX(at), SUM(n) FROM tasks GROUP BY done",
        "SELECT MIN(at), MAX(at), SUM(done), COUNT(at) FROM tasks",
        "SELECT done, MAX(at) FROM tasks WHERE at >= '2026-10-01' GROUP BY done HAVING MAX(at) > '2026-10-05'",
        "SELECT id, done, at FROM tasks WHERE at >= '2026-10-01' ORDER BY at DESC LIMIT 7",
        "SELECT id, done, at FROM tasks WHERE done IS NOT TRUE AND at IS NOT NULL ORDER BY at LIMIT 9",
        "SELECT id, at FROM tasks WHERE at IS NOT NULL ORDER BY decay(at, '2026-10-01T00:30:00Z', 7200) DESC LIMIT 6",
    ] {
        let mut one = whole.query(sql, &Params::new()).unwrap().rows;
        let mut two = sharded(&[&a, &b], sql).rows;
        if sql.contains("GROUP BY") {
            one.sort_by_key(|r| format!("{r:?}"));
            two.sort_by_key(|r| format!("{r:?}"));
        }
        assert_eq!(one, two, "{sql}");
        assert!(!one.is_empty(), "{sql}");
        // Every BOOL and TIMESTAMP value is answered as one.
        assert!(one.iter().flatten().all(|v| !matches!(v, Value::Int(x) if (*x).abs() > 1_000_000)), "{sql}: {one:?}");
        assert!(one.iter().flatten().any(|v| matches!(v, Value::Bool(_) | Value::Timestamp(_))), "{sql}: {one:?}");
    }
}

#[test]
fn an_unknown_feature_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d");
    let opts = || DbOptions { layout: Layout::Directory, sync: false, ..DbOptions::default() };
    Db::create(&path, tasks(), opts()).unwrap();
    assert_eq!(format_on_disk(&path), 3);

    // The manifest as a later version would write it, with a feature this
    // one lacks; CURRENT names it with its checksum.
    let mut cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
    let name = cur["manifest"].as_str().unwrap().to_string();
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join(&name)).unwrap()).unwrap();
    assert_eq!(manifest["features"]["required"], serde_json::json!(["bool", "timestamp"]));
    let mut rewrite = |m: &serde_json::Value| {
        let bytes = serde_json::to_vec(m).unwrap();
        std::fs::write(path.join(&name), &bytes).unwrap();
        cur["crc"] = crc32fast::hash(&bytes).into();
        std::fs::write(path.join("CURRENT"), serde_json::to_vec(&cur).unwrap()).unwrap();
    };
    manifest["features"]["optional"] = serde_json::json!(["page-cache-hints"]);
    rewrite(&manifest);
    Db::open(&path, opts()).unwrap();
    manifest["features"]["required"] = serde_json::json!(["bool", "geo", "timestamp"]);
    rewrite(&manifest);
    let e = Db::open(&path, opts()).err().expect("an unknown feature is refused").to_string();
    assert!(e.contains("\"geo\"") && e.contains("newer"), "{e}");
}
