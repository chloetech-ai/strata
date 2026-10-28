//! What a commit's result promises: `Ok` means the transaction is durable
//! and `Err` that it is not, so a caller may retry on `Err` without
//! writing twice. Increments read the transaction's own writes, and a
//! merge never erases an index it cannot rebuild.

use strata::db::Db;
use strata::query::Params;
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::{Field, FieldType, Schema, Value};

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("n", FieldType::Int),
            Field::new("x", FieldType::Float),
            Field::new("body", FieldType::Text { positions: false }),
        ],
        Some("id"),
    )
    .unwrap()
}

fn doc(id: &str, n: i64) -> Vec<Value> {
    vec![Value::Str(id.into()), Value::Int(n), Value::Float(0.5), Value::Str(format!("document {id}"))]
}

fn get(db: &Db, id: &str, field: &str) -> Value {
    let p = Params::new().set("k", Value::Str(id.into()));
    let r = db.query(&format!("SELECT {field} FROM t WHERE id = :k"), &p).unwrap();
    r.rows.first().map_or(Value::Null, |row| row[0].clone())
}

fn key(id: &str) -> Value {
    Value::Str(id.into())
}

// Bug 13: an `add` after an upsert of the same key in one transaction
// read the stored value instead of the upserted one, so the upsert's value
// was lost.
#[test]
fn adds_read_the_transactions_own_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("a.strata"), schema(), DbOptions::default()).unwrap();
    db.load(vec![doc("a", 100), doc("b", 100)]).unwrap();
    let s = db.storage();

    let mut tx = s.begin().unwrap();
    tx.upsert(doc("a", 1)).unwrap();
    assert!(tx.add(&key("a"), "n", &Value::Int(5)).unwrap());
    // Two adds and a set in between, on a key from an earlier commit.
    assert!(tx.add(&key("b"), "n", &Value::Int(1)).unwrap());
    assert!(tx.set(&key("b"), "n", Value::Int(10)).unwrap());
    assert!(tx.add(&key("b"), "n", &Value::Int(2)).unwrap());
    assert!(tx.add(&key("b"), "x", &Value::Float(0.25)).unwrap());
    // A key inserted in this transaction, and one that does not exist.
    tx.insert(doc("c", 7)).unwrap();
    assert!(tx.add(&key("c"), "n", &Value::Int(-8)).unwrap());
    assert!(!tx.add(&key("nope"), "n", &Value::Int(1)).unwrap());
    tx.commit().unwrap();
    assert_eq!(get(&db, "a", "n"), Value::Int(6));
    assert_eq!(get(&db, "b", "n"), Value::Int(12));
    assert_eq!(get(&db, "b", "x"), Value::Float(0.75));
    assert_eq!(get(&db, "c", "n"), Value::Int(-1));

    // Exact past 2^53, and refused on overflow or a fraction for an INT.
    let big = 1i64 << 53;
    let mut tx = s.begin().unwrap();
    tx.set(&key("a"), "n", Value::Int(big)).unwrap();
    tx.add(&key("a"), "n", &Value::Int(1)).unwrap();
    tx.commit().unwrap();
    assert_eq!(get(&db, "a", "n"), Value::Int(big + 1));
    let mut tx = s.begin().unwrap();
    tx.set(&key("a"), "n", Value::Int(i64::MAX)).unwrap();
    assert!(tx.add(&key("a"), "n", &Value::Int(1)).is_err());
    assert!(tx.add(&key("a"), "n", &Value::Float(0.5)).is_err());
    assert!(tx.add(&key("a"), "x", &Value::Float(f64::MAX)).is_ok());
    assert!(tx.add(&key("a"), "x", &Value::Float(f64::MAX)).is_err(), "a FLOAT sum stays finite");
    assert!(tx.add(&key("a"), "id", &Value::Int(1)).is_err());
}

// DB-2: a commit whose WAL append succeeded but whose memtable flush failed
// returned an error, so a caller retried a write that had happened (an
// increment applied twice). The commit is now Ok, the flush error is kept,
// and the next commit flushes.
#[test]
fn a_failed_flush_does_not_fail_a_durable_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.strata");
    let opts = || DbOptions { memtable_max_docs: 4, layout: Layout::Directory, ..Default::default() };
    let db = Db::create(&path, schema(), opts()).unwrap();
    db.load(vec![doc("a", 0)]).unwrap();
    // Directories where the next segment files go make every flush fail,
    // even for root.
    let blockers: Vec<_> = (0..64).map(|id| path.join(format!("seg-{id:010}.strata"))).filter(|p| !p.exists()).collect();
    for b in &blockers {
        std::fs::create_dir(b).unwrap();
    }
    let s = db.storage();
    let commit = |i: i64| {
        let mut tx = s.begin().unwrap();
        tx.add(&key("a"), "n", &Value::Int(1)).unwrap();
        tx.insert(doc(&format!("d{i}"), i)).unwrap();
        tx.commit()
    };
    // Commits pass the memtable's limit of 4 and their flushes fail, but
    // they are durable, so they are Ok.
    let mut ok = 0;
    while s.flush_error().is_none() {
        assert!(ok < 20, "no flush failed");
        commit(ok).unwrap();
        ok += 1;
    }
    commit(ok).unwrap();
    ok += 1;
    assert_eq!(get(&db, "a", "n"), Value::Int(ok));
    let r = db.query("SELECT COUNT(*) FROM t", &Params::new()).unwrap();
    assert_eq!(r.rows[0][0], Value::Int(1 + ok));

    // Writes wait once the memtable holds twice its limit and flushes
    // still fail: refused before they are logged, so not applied.
    let mut refused = 0;
    for i in ok..ok + 20 {
        match commit(i) {
            Ok(()) => assert_eq!(refused, 0, "a write got through after one was refused"),
            Err(_) => refused += 1,
        }
    }
    assert!(refused > 0);
    let applied = ok + 20 - refused;
    assert_eq!(get(&db, "a", "n"), Value::Int(applied));

    for b in &blockers {
        std::fs::remove_dir(b).unwrap();
    }
    commit(1000).unwrap();
    s.flush().unwrap();
    assert_eq!(s.flush_error(), None);
    let want = get(&db, "a", "n");
    assert_eq!(want, Value::Int(applied + 1));
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(get(&db, "a", "n"), want);
}

// DB-3: an indexed TEXT field that was neither stored nor generated lost
// its index at the first merge, since a merge rebuilds indexes from stored
// values. Schemas now refuse such a field, and a merge of a database
// created before that fails instead of erasing the index.
#[test]
fn an_index_a_merge_cannot_rebuild_is_refused() {
    let unstored = |f: Field| Schema::new("t", vec![Field::new("id", FieldType::Keyword), f], Some("id"));
    let text = FieldType::Text { positions: false };
    assert!(unstored(Field::new("body", text.clone()).not_stored()).is_err());
    assert!(unstored(Field::new("body", FieldType::Sparse).not_stored()).is_err());
    assert!(unstored(Field::new("body", text.clone()).stored_only()).is_ok());
    let generated = Schema::new(
        "t",
        vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Keyword), Field::new("body", text.clone()).not_stored().generated(&["title"])],
        Some("id"),
    );
    assert!(generated.is_ok());

    // A schema written before the check, built field by field.
    let mut old = schema();
    old.fields[3].stored = false;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("m.strata"), old, DbOptions { memtable_max_docs: 2, ..Default::default() }).unwrap();
    for i in 0..6 {
        let mut tx = db.storage().begin().unwrap();
        tx.insert(doc(&format!("d{i}"), i)).unwrap();
        tx.commit().unwrap();
    }
    db.storage().flush().unwrap();
    let hits = |db: &Db| db.query("SELECT COUNT(*) FROM t WHERE MATCH(body, 'document')", &Params::new()).unwrap().rows[0][0].clone();
    assert_eq!(hits(&db), Value::Int(6));
    let policy = MergePolicy { merge_factor: 2, ..Default::default() };
    let r = db.storage().merge(&policy);
    assert!(matches!(&r, Err(e) if e.to_string().contains("erase")), "{r:?}");
    assert_eq!(hits(&db), Value::Int(6));
}
