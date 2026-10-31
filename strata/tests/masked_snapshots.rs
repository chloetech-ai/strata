//! Masked snapshots (`Snapshot::masked`): rows a mask hides, by primary
//! key or by a table's `shard_by` value, are left out of queries, counts
//! and lookups, on disk and in the memtable, while a write by key still
//! replaces a hidden row.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use strata::db::Db;
use strata::query::Params;
use strata::storage::{DbOptions, Mask};
use strata::value::Value;
use strata::{Field, FieldType, Schema};

fn count(db: &Db, snap: &strata::storage::Snapshot, sql: &str) -> i64 {
    db.query_at(snap, sql, &Params::new(), &db.exec).unwrap().rows[0][0].as_i64().unwrap()
}

fn ids(db: &Db, snap: &strata::storage::Snapshot, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = db.query_at(snap, sql, &Params::new(), &db.exec).unwrap().rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

#[test]
fn masked_snapshots_hide_rows_by_placement() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::new("items", vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: true }), Field::new("n", FieldType::Int)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("m.strata"), schema, DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    let values: Vec<String> = (0..40).map(|i| format!("('i{i:02}', 'red kettle number {i}', {i})")).collect();
    db.sql(&format!("INSERT INTO items (id, body, n) VALUES {}", values.join(", ")), &Params::new()).unwrap();
    db.storage().flush().unwrap();
    // Some rows in the memtable too.
    db.sql("INSERT INTO items (id, body, n) VALUES ('j00', 'red kettle late', 100), ('j01', 'red kettle late', 101)", &Params::new()).unwrap();
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, ts INT) OPTIONS(shard_by = 'usr')", &Params::new()).unwrap();
    db.sql("INSERT INTO events (id, usr, ts) VALUES (1, 'ann', 1), (2, 'bob', 2), (3, 'ann', 3), (4, 'cid', 4)", &Params::new()).unwrap();

    // Hide items whose key ends in an even digit, and bob's events.
    let asked = Arc::new(AtomicUsize::new(0));
    let a = asked.clone();
    let mask = Mask::new(move |schema, v| {
        a.fetch_add(1, Ordering::Relaxed);
        match (schema.table.as_str(), v) {
            ("items", Value::Str(s)) => s.as_bytes()[s.len() - 1] % 2 == 0,
            ("events", Value::Str(s)) => s == "bob",
            _ => false,
        }
    });
    let snap = db.snapshot();
    let masked = snap.masked(&mask).unwrap();
    assert_eq!(count(&db, &snap, "SELECT COUNT(*) FROM items"), 42);
    assert_eq!(count(&db, &masked, "SELECT COUNT(*) FROM items"), 21);
    assert_eq!(masked.n_live_docs(), 21);
    let found = ids(&db, &masked, "SELECT id FROM items WHERE MATCH(body, 'kettle') LIMIT 100");
    assert_eq!(found.len(), 21);
    assert!(found.iter().all(|id| id.as_bytes()[id.len() - 1] % 2 == 1), "{found:?}");
    assert_eq!(ids(&db, &masked, "SELECT id FROM items ORDER BY n DESC LIMIT 1"), vec!["j01"]);
    assert_eq!(ids(&db, &masked, "SELECT id FROM items LIMIT 100").len(), 21);
    assert_eq!(ids(&db, &masked, "SELECT id, n FROM items LIMIT 100").len(), 21);
    assert!(masked.lookup(&Value::Str("i02".into())).is_none() && masked.lookup(&Value::Str("i03".into())).is_some());
    assert!(snap.lookup(&Value::Str("i02".into())).is_some(), "the snapshot masked is unchanged");
    let events = masked.table("events").unwrap();
    assert_eq!(events.n_live_docs(), 3);
    assert_eq!(count(&db, &masked, "SELECT COUNT(*) FROM events WHERE usr = 'bob'"), 0);
    assert_eq!(count(&db, &masked, "SELECT COUNT(*) FROM events"), 3);

    // Kept per segment: a second snapshot of the same segments asks no
    // more.
    let n = asked.load(Ordering::Relaxed);
    let again = db.snapshot().masked(&mask).unwrap();
    assert_eq!(asked.load(Ordering::Relaxed), n);
    assert_eq!(count(&db, &again, "SELECT COUNT(*) FROM items"), 21);

    // A write by key replaces a hidden row: no second copy appears when
    // nothing is hidden.
    db.sql("INSERT OR REPLACE INTO items (id, body, n) VALUES ('i02', 'blue lamp', 7)", &Params::new()).unwrap();
    let after = db.snapshot();
    assert_eq!(count(&db, &after, "SELECT COUNT(*) FROM items"), 42);
    assert_eq!(count(&db, &after.masked(&mask).unwrap(), "SELECT COUNT(*) FROM items"), 21);
    assert_eq!(count(&db, &after, "SELECT COUNT(*) FROM items WHERE MATCH(body, 'lamp')"), 1);
}

/// What a shard moving rows reads: the rows a mask covers, those changed
/// since a snapshot, those gone since, rebuilt whole; and statements that
/// pass over the rows a mask hides.
#[test]
fn covered_rows_changes_and_masked_writes() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::new("items", vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: true }), Field::new("n", FieldType::Int)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("c.strata"), schema, DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.storage().start_seq_no().unwrap();
    let values: Vec<String> = (0..20).map(|i| format!("('i{i:02}', 'red kettle number {i}', {i})")).collect();
    db.sql(&format!("INSERT INTO items (id, body, n) VALUES {}", values.join(", ")), &Params::new()).unwrap();
    db.storage().flush().unwrap();
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, ts INT) OPTIONS(shard_by = 'usr')", &Params::new()).unwrap();
    db.sql("INSERT INTO events (id, usr, ts) VALUES (1, 'ann', 1), (2, 'bob', 2), (3, 'ann', 3), (4, 'cid', 4)", &Params::new()).unwrap();
    // The moving rows: items of an even last digit, and ann's events.
    let moving = Mask::new(|schema, v| match (schema.table.as_str(), v) {
        ("items", Value::Str(s)) => s.as_bytes()[s.len() - 1] % 2 == 0,
        ("events", Value::Str(s)) => s == "ann",
        _ => false,
    });
    let keys = |snap: &strata::storage::Snapshot, rows: &[(usize, u32)]| -> Vec<String> {
        let mut k: Vec<String> = db
            .documents_of(snap, rows)
            .unwrap()
            .iter()
            .map(|d| match &d[0] {
                Value::Str(s) => s.clone(),
                v => v.as_i64().unwrap().to_string(),
            })
            .collect();
        k.sort();
        k
    };
    let s0 = db.snapshot();
    let items0 = s0.table("items").unwrap();
    let all = items0.covered(&moving, None).unwrap();
    assert_eq!(all.len(), 10);
    let docs = db.documents_of(&items0, &all).unwrap();
    assert!(docs.iter().all(|d| d.len() == 3 && matches!(&d[1], Value::Str(b) if b.starts_with("red kettle")) && d[2].as_i64().is_some()), "{docs:?}");
    let events0 = s0.table("events").unwrap();
    assert_eq!(keys(&events0, &events0.covered(&moving, None).unwrap()), vec!["1", "3"]);

    // Changes: an update in place, a rewrite, a delete, an insert, a row
    // moving out (ann's event 3 becomes cid's), and rows that do not move.
    db.sql("UPDATE items SET n = n + 100 WHERE id = 'i02'", &Params::new()).unwrap();
    db.sql("INSERT OR REPLACE INTO items (id, body, n) VALUES ('i04', 'blue lamp', 4), ('i05', 'blue lamp', 5)", &Params::new()).unwrap();
    db.sql("DELETE FROM items WHERE id IN ('i06', 'i07')", &Params::new()).unwrap();
    db.sql("INSERT INTO items (id, body, n) VALUES ('j00', 'new', 0), ('j01', 'new', 1)", &Params::new()).unwrap();
    db.sql("DELETE FROM events WHERE id = 3", &Params::new()).unwrap();
    db.sql("INSERT INTO events (id, usr, ts) VALUES (3, 'cid', 30), (5, 'ann', 5)", &Params::new()).unwrap();
    let s1 = db.snapshot();
    let after = s0.seq_no();
    let items1 = s1.table("items").unwrap();
    assert_eq!(keys(&items1, &items1.covered(&moving, after).unwrap()), vec!["i02", "i04", "j00"]);
    let changed = db.documents_of(&items1, &items1.covered(&moving, after).unwrap()).unwrap();
    assert!(changed.iter().any(|d| d[0] == Value::Str("i02".into()) && d[2] == Value::Int(102)), "{changed:?}");
    let gone: Vec<Value> = items1.covered_gone(&items0, &moving).unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(gone, vec![Value::Str("i06".into())]);
    let events1 = s1.table("events").unwrap();
    assert_eq!(keys(&events1, &events1.covered(&moving, after).unwrap()), vec!["5"]);
    assert_eq!(events1.covered_gone(&events0, &moving).unwrap(), vec![(Value::Int(3), Value::Str("ann".into()))]);
    assert!(db.snapshot().table("events").unwrap().covered_gone(&events1, &moving).unwrap().is_empty());

    // Statements pass over the rows their guard's mask hides; writes by
    // key do not. A guard that refuses stops the write.
    let hide = Arc::new(Mask::new(|schema, v| schema.table == "items" && matches!(v, Value::Str(s) if s.starts_with('i'))));
    let guarded = |sql: &str| {
        let h = hide.clone();
        let w = db.prepare_write(sql, &Params::new()).unwrap().unwrap().guarded(move || Ok(Some(h.clone())));
        db.write_prepared(&w)
    };
    assert_eq!(guarded("UPDATE items SET n = 7").unwrap(), 2);
    guarded("DELETE FROM items WHERE n >= 0").unwrap();
    guarded("INSERT OR REPLACE INTO items (id, body, n) VALUES ('i01', 'by key', -1)").unwrap();
    let refused = db.prepare_write("DELETE FROM items", &Params::new()).unwrap().unwrap().guarded(|| Err(strata::Error::Busy("the rows moved".into())));
    assert!(matches!(db.write_prepared(&refused), Err(strata::Error::Busy(_))));
    let snap = db.snapshot();
    assert_eq!(count(&db, &snap, "SELECT COUNT(*) FROM items"), 18);
    assert_eq!(count(&db, &snap, "SELECT COUNT(*) FROM items WHERE n = -1"), 1);
    assert_eq!(count(&db, &snap, "SELECT COUNT(*) FROM items WHERE n = 7"), 0);
}
