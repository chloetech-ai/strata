//! Strata as a key-value store: rows read by primary key without SQL
//! (`Db::get`, `Db::get_many_at`), the memtable's newest chunk read as
//! written rather than built per commit, and rows that expire at a time
//! they hold (`expire_by`).

use std::collections::BTreeMap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::storage::{Applied, DbOptions, SeqNo};
use strata::{Field, FieldType, Schema, Value};

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("n", FieldType::Int),
            Field::new("body", FieldType::Text { positions: false }),
            Field { stored: false, ..Field::new("note", FieldType::Int) },
            Field::boolean("flag"),
        ],
        Some("id"),
    )
    .unwrap()
}

fn key(i: u64) -> String {
    format!("k{i:04}")
}

fn row(i: u64, v: i64) -> Vec<Value> {
    vec![Value::Str(key(i)), Value::Int(v), Value::Str(format!("row {i} version {v}")), Value::Int(v * 3), Value::Bool(v % 2 == 0)]
}

/// What `SELECT *` answers for key `k`.
fn by_sql(db: &Db, k: &str) -> Option<Vec<Value>> {
    let rs = db.query("SELECT * FROM t WHERE id = :k", &Params::new().set("k", Value::Str(k.into()))).unwrap();
    assert!(rs.rows.len() <= 1);
    rs.rows.into_iter().next()
}

fn options() -> DbOptions {
    DbOptions { memtable_max_docs: 1_000_000, memtable_chunk_docs: 32, segment_max_docs: 100, sync: false, publish_snapshots: true, ..DbOptions::default() }
}

/// Reads by key answer what SQL answers, for rows in the memtable's
/// newest chunk (read as written), in its sealed chunks and on disk,
/// through upserts, deletes and values set in place, one write per
/// commit.
#[test]
fn reads_by_key_answer_what_sql_answers() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("kv.strata"), schema(), options()).unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let mut model: BTreeMap<String, i64> = BTreeMap::new();
    let keys = 120;
    for step in 0..1500u64 {
        let i = rng.gen_range(0..keys);
        let k = key(i);
        let mut tx = db.storage().begin().unwrap();
        match rng.gen_range(0..10) {
            0..=4 => {
                let v = rng.gen_range(0..1000);
                tx.upsert(row(i, v)).unwrap();
                model.insert(k, v);
            }
            5 | 6 => {
                let v = rng.gen_range(0..1000);
                let ok = tx.insert(row(i, v)).is_ok();
                assert_eq!(ok, !model.contains_key(&k), "insert of {k}");
                if ok {
                    model.insert(k, v);
                }
            }
            7 => {
                assert_eq!(tx.delete(&Value::Str(k.clone())).unwrap(), model.remove(&k).is_some(), "delete of {k}");
            }
            _ => {
                let v = rng.gen_range(0..1000);
                let had = model.contains_key(&k);
                assert_eq!(tx.set(&Value::Str(k.clone()), "n", Value::Int(v)).unwrap(), had, "set of {k}");
                if had {
                    model.insert(k, v);
                }
            }
        }
        tx.commit().unwrap();
        if step % 400 == 399 {
            db.storage().flush().unwrap();
        }
        // A read by key after every write, and SQL now and then (which
        // builds the newest chunk).
        let probe = key(rng.gen_range(0..keys));
        let got = db.get("t", &Value::Str(probe.clone())).unwrap();
        assert_eq!(got.as_ref().map(|r| r[1].clone()), model.get(&probe).map(|&v| Value::Int(v)), "step {step}: get {probe}");
        if step % 7 == 0 {
            assert_eq!(got, by_sql(&db, &probe), "step {step}: get {probe} against SQL");
        }
    }
    let all: Vec<Value> = (0..keys + 5).map(|i| Value::Str(key(i))).collect();
    let got = db.get_many("t", &all).unwrap();
    for (k, g) in all.iter().zip(&got) {
        let k = k.as_str().unwrap();
        assert_eq!(g.as_ref().map(|r| r[1].clone()), model.get(k).map(|&v| Value::Int(v)), "get_many {k}");
        assert_eq!(g.clone(), by_sql(&db, k), "get_many {k} against SQL");
    }
    let count = db.query("SELECT COUNT(*) FROM t", &Params::new()).unwrap().rows[0][0].clone();
    assert_eq!(count, Value::Int(model.len() as i64));
    // Typed as SQL types them.
    let k = model.keys().next().unwrap();
    let r = db.get("t", &Value::Str(k.clone())).unwrap().unwrap();
    assert!(matches!(r[3], Value::Bool(_)), "{r:?}");
    assert_eq!(Db::get_columns(db.snapshot().table("t").unwrap().schema()), ["id", "n", "body", "flag"]);
}

/// The newest chunk is read as written: a commit does not rebuild it,
/// queries still find every row, and an INT key may come as text.
#[test]
fn single_row_commits_leave_the_newest_chunk_unbuilt() {
    let dir = tempfile::tempdir().unwrap();
    let s = Schema::new("c", vec![Field::new("id", FieldType::Int), Field::new("v", FieldType::Int)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("c.strata"), s, DbOptions { memtable_chunk_docs: 1024, ..options() }).unwrap();
    for i in 0..3000i64 {
        let mut tx = db.storage().begin().unwrap();
        tx.insert(vec![Value::Int(i), Value::Int(i * 10)]).unwrap();
        tx.commit().unwrap();
        if i % 500 == 0 {
            assert_eq!(db.get("c", &Value::Int(i)).unwrap(), Some(vec![Value::Int(i), Value::Int(i * 10)]));
        }
    }
    assert_eq!(db.get("c", &Value::Str("2999".into())).unwrap(), Some(vec![Value::Int(2999), Value::Int(29990)]));
    assert!(db.get("c", &Value::Str("x".into())).is_err());
    let rs = db.query("SELECT COUNT(*) FROM c WHERE v >= 10000", &Params::new()).unwrap();
    assert_eq!(rs.rows[0][0], Value::Int(2000));
    let rs = db.query("SELECT id FROM c ORDER BY v DESC LIMIT 2", &Params::new()).unwrap();
    assert_eq!(rs.rows, vec![vec![Value::Int(2999)], vec![Value::Int(2998)]]);
}

/// Each row read by key carries the number of the write that last
/// changed it, as `seq_no()` gives it.
#[test]
fn reads_by_key_carry_sequence_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("s.strata"), schema(), options()).unwrap();
    db.storage().start_seq_no().unwrap();
    for i in 0..80 {
        let mut tx = db.storage().begin().unwrap();
        tx.upsert(row(i % 50, i as i64)).unwrap();
        tx.commit().unwrap();
        if i == 40 {
            db.storage().flush().unwrap();
        }
    }
    let snap = db.snapshot().table("t").unwrap();
    let keys: Vec<Value> = (0..50).map(|i| Value::Str(key(i))).collect();
    let got = db.get_many_at(&snap, &keys, &ExecOptions::default()).unwrap();
    for (k, g) in keys.iter().zip(got) {
        let rs = db.query("SELECT seq_no() FROM t WHERE id = :k", &Params::new().set("k", k.clone())).unwrap();
        assert_eq!(Value::Int(g.unwrap().seq_no as i64), rs.rows[0][0], "{k:?}");
    }
}

fn ttl_schema() -> Schema {
    Schema::new("s", vec![Field::new("id", FieldType::Keyword), Field::new("v", FieldType::Int), Field::timestamp("until")], Some("id")).unwrap().with_expire_by("until").unwrap()
}

const HOUR: i64 = 3_600_000_000;

fn count(db: &Db, sql: &str) -> i64 {
    db.query(sql, &Params::new()).unwrap().rows[0][0].as_i64().unwrap()
}

/// Rows expire at the time they hold: queries, counts and reads by key
/// leave them out at once; a write by the key finds no row; `Db::expire`
/// deletes them by logged deletes, which a replica applies.
#[test]
fn rows_expire_at_their_time() {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { ship_log_bytes: 1 << 30, ..options() };
    let db = Db::create(&dir.path().join("p.strata"), ttl_schema(), opts.clone()).unwrap();
    db.storage().start_seq_no().unwrap();
    let now = strata::timestamp::now();
    let mut tx = db.storage().begin().unwrap();
    for i in 0..40u64 {
        // Odd keys expired an hour ago; even ones expire in an hour, and
        // every tenth never.
        let until = match i {
            _ if i % 10 == 0 => Value::Null,
            _ if i % 2 == 1 => Value::Timestamp(now - HOUR),
            _ => Value::Timestamp(now + HOUR),
        };
        tx.insert(vec![Value::Str(key(i)), Value::Int(i as i64), until]).unwrap();
    }
    tx.commit().unwrap();
    let live = 20;
    assert_eq!(count(&db, "SELECT COUNT(*) FROM s"), live);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM s WHERE v >= 0"), live);
    assert_eq!(db.get("s", &Value::Str(key(1))).unwrap(), None);
    assert_eq!(db.get("s", &Value::Str(key(2))).unwrap().unwrap()[1], Value::Int(2));
    // A later time sees the even ones go too, an option keeps them all.
    let snap = db.snapshot().table("s").unwrap();
    let later = ExecOptions { now: Some(now + 2 * HOUR), ..ExecOptions::default() };
    let rs = db.query_at(&snap, "SELECT COUNT(*) FROM s", &Params::new(), &later).unwrap();
    assert_eq!(rs.rows[0][0], Value::Int(4));
    let all = ExecOptions { expired: true, ..ExecOptions::default() };
    let rs = db.query_at(&snap, "SELECT COUNT(*) FROM s", &Params::new(), &all).unwrap();
    assert_eq!(rs.rows[0][0], Value::Int(40));
    assert!(db.get_many_at(&snap, &[Value::Str(key(1))], &all).unwrap()[0].is_some());
    // An UPDATE finds no expired row; an INSERT of its key succeeds; a
    // delete of it says there was none.
    let SqlResult::Written(n) = db.sql("UPDATE s SET v = 99 WHERE id = 'k0003'", &Params::new()).unwrap() else { panic!() };
    assert_eq!(n, 0);
    let mut tx = db.storage().begin().unwrap();
    tx.insert(vec![Value::Str(key(3)), Value::Int(300), Value::Null]).unwrap();
    assert!(!tx.delete(&Value::Str(key(5))).unwrap());
    assert!(!tx.set(&Value::Str(key(7)), "v", Value::Int(1)).unwrap());
    tx.commit().unwrap();
    assert_eq!(db.get("s", &Value::Str(key(3))).unwrap().unwrap()[1], Value::Int(300));
    assert_eq!(count(&db, "SELECT COUNT(*) FROM s"), live + 1);
    // Setting the time in place renews a row.
    let mut tx = db.storage().begin().unwrap();
    assert!(tx.set(&Value::Str(key(2)), "until", Value::Timestamp(now - HOUR)).unwrap());
    tx.commit().unwrap();
    assert_eq!(db.get("s", &Value::Str(key(2))).unwrap(), None);
    // The expirer deletes what is left of the expired rows, and a replica
    // that applies the log holds the same rows.
    db.storage().flush().unwrap();
    let raw = |db: &Db| db.documents(&db.snapshot().table("s").unwrap()).count();
    let before = raw(&db);
    let gone = db.expire(strata::timestamp::now(), 5).unwrap();
    assert_eq!(gone, 18, "the odd keys but k0003, k0005 and k0007, and k0002");
    assert_eq!(raw(&db), before - 18);
    assert_eq!(db.expire(strata::timestamp::now(), 5).unwrap(), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM s"), live);

    let replica = Db::create(&dir.path().join("r.strata"), ttl_schema(), opts).unwrap();
    replica.storage().start_seq_no().unwrap();
    let log = match db.storage().log_from(1, usize::MAX) {
        strata::storage::LogRead::Txns(t) => t,
        _ => panic!("the log holds every transaction"),
    };
    let bytes: Vec<&[u8]> = log.iter().map(|t| &*t.bytes).collect();
    let end = log.last().unwrap().seq;
    let start = SeqNo { term: db.storage().numbering().unwrap().term_at(0), seq: 0 };
    assert_eq!(replica.storage().apply_shipped(start, &bytes).unwrap(), Applied::At(end));
    let rows = |db: &Db| {
        let mut r: Vec<_> = db.documents(&db.snapshot().table("s").unwrap()).map(|d| d.unwrap()).collect();
        r.sort_by_key(|d| d[0].as_str().unwrap().to_string());
        r
    };
    assert_eq!(rows(&replica), rows(&db));
}

/// `CREATE TABLE ... OPTIONS(expire_by = ...)` names an indexed
/// TIMESTAMP of a table with a key; the option survives reopening, and a
/// column it names is not dropped.
#[test]
fn expire_by_is_a_table_option() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.strata");
    let db = Db::create(&path, ttl_schema(), options()).unwrap();
    db.sql("CREATE TABLE sessions (id KEYWORD PRIMARY KEY, until TIMESTAMP) OPTIONS(expire_by = 'until')", &Params::new()).unwrap();
    for (sql, why) in [
        ("CREATE TABLE a (id KEYWORD PRIMARY KEY, until INT) OPTIONS(expire_by = 'until')", "TIMESTAMP"),
        ("CREATE TABLE b (id KEYWORD, until TIMESTAMP) OPTIONS(expire_by = 'until')", "primary key"),
        ("CREATE TABLE c (id KEYWORD PRIMARY KEY, until TIMESTAMP) OPTIONS(expire_by = 'when')", "not a field"),
    ] {
        let e = db.sql(sql, &Params::new()).unwrap_err().to_string();
        assert!(e.contains(why), "{sql}: {e}");
    }
    let e = db.sql("ALTER TABLE sessions DROP COLUMN until", &Params::new()).unwrap_err().to_string();
    assert!(e.contains("expire_by"), "{e}");
    db.sql("ALTER TABLE sessions RENAME COLUMN until TO ends", &Params::new()).unwrap();
    db.sql("INSERT INTO sessions (id, ends) VALUES ('a', '2000-01-01T00:00:00Z'), ('b', '2999-01-01T00:00:00Z')", &Params::new()).unwrap();
    drop(db);
    let db = Db::open(&path, options()).unwrap();
    let snap = db.snapshot().table("sessions").unwrap();
    assert_eq!(snap.schema().expire_by.as_deref(), Some("ends"));
    assert!(snap.schema().features().any(|f| f == "expire_by"));
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 1);
}
