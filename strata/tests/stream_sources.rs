//! The system table `_sources`: a stream source's last offset, written in
//! the transaction of the rows its events made, kept across a reopen and
//! in checkpoints, and kept out of the database's tables.

use strata::db::{Db, Source};
use strata::query::Params;
use strata::storage::{CheckpointOptions, DbOptions, Layout};
use strata::value::Value;
use strata::Schema;

fn schema() -> Schema {
    Schema::from_json(r#"{"table": "events", "key": "id", "fields": [{"name": "id", "type": "int"}, {"name": "usr", "type": "keyword"}]}"#).unwrap()
}

fn count(db: &Db, table: &str) -> i64 {
    db.query(&format!("SELECT COUNT(*) FROM {table}"), &Params::new()).unwrap().rows[0][0].as_i64().unwrap()
}

fn offsets(db: &Db) -> Vec<(String, i64)> {
    db.sources().unwrap().into_iter().map(|Source { name, offset, .. }| (name, offset)).collect()
}

#[test]
fn offsets_commit_with_their_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.strata");
    let db = Db::create(&path, schema(), DbOptions::default()).unwrap();
    assert!(db.sources().unwrap().is_empty());
    db.storage().ensure_sources().unwrap();
    // Made once; not one of the tables, nor one a client makes or drops.
    db.storage().ensure_sources().unwrap();
    assert_eq!(db.catalog().len(), 1);
    let e = db.sql("CREATE TABLE _sources (name KEYWORD PRIMARY KEY)", &Params::new()).err().unwrap();
    assert!(e.to_string().contains("system table"), "{e}");
    let e = db.drop_table("_sources").err().unwrap();
    assert!(e.to_string().contains("cannot be dropped"), "{e}");

    let mut t = db.storage().begin().unwrap();
    assert_eq!(t.source_offset("clicks/0").unwrap(), None);
    for i in 0..10 {
        t.insert(vec![Value::Int(i), Value::Str("u1".into())]).unwrap();
    }
    t.set_source("clicks/0", 9, 1_000).unwrap();
    // The transaction sees its own offset.
    assert_eq!(t.source_offset("clicks/0").unwrap(), Some(9));
    t.commit().unwrap();

    // Not committed: neither the rows nor the offset.
    let mut t = db.storage().begin().unwrap();
    t.insert(vec![Value::Int(10), Value::Str("u2".into())]).unwrap();
    t.set_source("clicks/0", 10, 2_000).unwrap();
    drop(t);
    assert_eq!((count(&db, "events"), offsets(&db)), (10, vec![("clicks/0".into(), 9)]));
    assert_eq!(db.sources().unwrap()[0].updated, Some(1_000));

    // A table made after the sources table, then a reopen and checkpoints
    // in both layouts: the offsets travel with the rows.
    db.sql("CREATE TABLE users (id KEYWORD PRIMARY KEY, n INT DEFAULT 0)", &Params::new()).unwrap();
    let mut t = db.storage().begin().unwrap();
    t.table("users").unwrap().insert(vec![Value::Str("u1".into()), Value::Int(3)]).unwrap();
    t.set_source("clicks/1", 4, 3_000).unwrap();
    t.commit().unwrap();
    drop(db);
    let db = Db::open(&path, DbOptions::default()).unwrap();
    let want = vec![("clicks/0".to_string(), 9), ("clicks/1".to_string(), 4)];
    assert_eq!(offsets(&db), want);
    assert_eq!(db.catalog().iter().map(|t| t.table.as_str()).collect::<Vec<_>>(), ["events", "users"]);
    for layout in [Layout::SingleFile, Layout::Directory] {
        let copy = dir.path().join(format!("copy-{layout:?}"));
        db.storage().checkpoint(&copy, &CheckpointOptions { layout, ..CheckpointOptions::default() }).unwrap();
        let c = Db::open(&copy, DbOptions::default()).unwrap();
        assert_eq!(offsets(&c), want, "{layout:?}");
        assert_eq!((count(&c, "events"), count(&c, "users")), (10, 1), "{layout:?}");
        // The copy's tables after the sources table are its tables still.
        let mut t = c.storage().begin().unwrap();
        t.table("users").unwrap().insert(vec![Value::Str("u2".into()), Value::Int(1)]).unwrap();
        t.commit().unwrap();
        assert_eq!(count(&c, "users"), 2);
    }
}
