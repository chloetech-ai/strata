//! Table options `cluster_by` and `retain`: each segment's rows ordered by
//! a field when a flush writes it and when a merge rewrites it, and rows
//! past their table's retention dropped.

use std::time::{Duration, Instant};

use strata::db::Db;
use strata::query::Params;
use strata::storage::{DbOptions, MergePolicy, MergerOptions};
use strata::value::Value;
use strata::Schema;

const DAY: i64 = 86_400_000_000;

fn events(options: &str) -> Schema {
    Schema::from_json(&format!(
        r#"{{"table": "events", "key": "id", {options} "fields": [
            {{"name": "id", "type": "int"}}, {{"name": "usr", "type": "keyword"}},
            {{"name": "n", "type": "int"}}, {{"name": "ts", "type": "timestamp"}}]}}"#
    ))
    .unwrap()
}

fn row(id: i64, usr: &str, ts: Option<i64>) -> Vec<Value> {
    vec![Value::Int(id), Value::Str(usr.into()), Value::Int(0), ts.map_or(Value::Null, Value::Timestamp)]
}

fn usr(i: i64) -> String {
    // Users in an order unlike that of the ids.
    format!("u{:02}", (i * 7919) % 37)
}

/// Each segment's rows as stored: (id, usr).
fn segments(db: &Db) -> Vec<Vec<(i64, String)>> {
    let snap = db.snapshot();
    let sizes: Vec<usize> = snap.segments().iter().map(|s| s.n_live as usize).collect();
    let mut docs = db.documents(&snap).map(|d| d.unwrap());
    sizes
        .iter()
        .map(|&n| {
            (0..n)
                .map(|_| {
                    let d = docs.next().unwrap();
                    let Value::Str(u) = &d[1] else { panic!("{d:?}") };
                    (d[0].as_i64().unwrap(), u.clone())
                })
                .collect()
        })
        .collect()
}

fn count(db: &Db, sql: &str) -> i64 {
    db.query(sql, &Params::new()).unwrap().rows[0][0].as_i64().unwrap()
}

fn ids(db: &Db, sql: &str) -> Vec<i64> {
    db.query(sql, &Params::new()).unwrap().rows.iter().map(|r| r[0].as_i64().unwrap()).collect()
}

fn write(db: &Db, rows: impl IntoIterator<Item = Vec<Value>>) {
    let mut t = db.storage().begin().unwrap();
    for r in rows {
        t.insert(r).unwrap();
    }
    t.commit().unwrap();
}

#[test]
fn clustered_segments_hold_each_value_in_one_run() {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { memtable_max_docs: usize::MAX, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("c.strata"), events(r#""cluster_by": "usr","#), opts.clone()).unwrap();
    let plain = Db::create(&dir.path().join("p.strata"), events(""), opts).unwrap();
    assert_eq!(db.schema().features().collect::<Vec<_>>(), ["timestamp", "cluster_by"]);
    for b in 0..6 {
        for d in [&db, &plain] {
            write(d, (b * 500..b * 500 + 500).map(|i| row(i, &usr(i), Some(i))));
            d.storage().flush().unwrap();
        }
    }
    // Each segment is sorted by user, and one user's rows keep their order
    // of writing.
    let sorted = |seg: &[(i64, String)]| seg.windows(2).all(|w| (&w[0].1, w[0].0) < (&w[1].1, w[1].0));
    let segs = segments(&db);
    assert_eq!(segs.len(), 6);
    assert!(segs.iter().all(|s| sorted(s)), "{segs:?}");
    assert!(!segments(&plain).iter().all(|s| sorted(s)));

    // Deletes and values written in place while a merge builds land on the
    // rows they were made to, wherever the merge put them.
    let policy = MergePolicy { merge_factor: 6, floor_docs: 10, ..MergePolicy::default() };
    for d in [&db, &plain] {
        let mut once = true;
        d.storage()
            .merge_with(&policy, || {
                if std::mem::take(&mut once) {
                    let mut t = d.storage().begin().unwrap();
                    for id in (0..3000).filter(|i| i % 10 == 3) {
                        assert!(t.delete(&Value::Int(id)).unwrap());
                    }
                    for id in (0..3000).filter(|i| i % 10 == 7) {
                        assert!(t.set(&Value::Int(id), "n", Value::Int(id)).unwrap());
                    }
                    t.commit().unwrap();
                }
                true
            })
            .unwrap();
        d.storage().merge(&policy).unwrap();
    }
    let segs = segments(&db);
    assert_eq!(segs.len(), 1, "merged into one");
    assert!(sorted(&segs[0]));
    for q in [
        "SELECT COUNT(*) FROM events",
        "SELECT SUM(n) FROM events",
        "SELECT COUNT(*) FROM events WHERE n > 0",
    ] {
        assert_eq!(count(&db, q), count(&plain, q), "{q}");
    }
    assert_eq!(count(&db, "SELECT COUNT(*) FROM events"), 2700);
    for u in ["u00", "u05", "u36"] {
        let q = format!("SELECT id FROM events WHERE usr = '{u}' ORDER BY id");
        assert_eq!(ids(&db, &q), ids(&plain, &q), "{q}");
        // The rows of one user, as stored, are its rows in id order.
        let stored: Vec<i64> = segs[0].iter().filter(|(_, x)| x == u).map(|(i, _)| *i).collect();
        assert_eq!(stored, ids(&plain, &q));
    }
    // The ordering field is not changed in place.
    let mut t = db.storage().begin().unwrap();
    let e = t.set(&Value::Int(1), "usr", Value::Str("u01".into())).unwrap_err();
    assert!(e.to_string().contains("cluster_by"), "{e}");
    drop(t);

    // The order is kept across a reopen; a table made by SQL takes it too.
    drop(db);
    let db = Db::open(&dir.path().join("c.strata"), DbOptions::default()).unwrap();
    assert!(sorted(&segments(&db)[0]));
    db.sql("CREATE TABLE views (id INT PRIMARY KEY, usr KEYWORD, ts TIMESTAMP) OPTIONS(cluster_by = 'usr', retain = '30 days')", &Params::new()).unwrap();
    let views = db.catalog().into_iter().find(|t| t.table == "views").unwrap();
    assert_eq!((views.cluster_by.as_deref(), views.retain.as_ref().map(|r| (r.by.as_str(), r.seconds))), (Some("usr"), Some(("ts", 30 * 86_400))));
    assert_eq!(views.to_short_json()["retain"], "30 days");
}

#[test]
fn rows_past_their_retention_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.strata");
    let opts = DbOptions { memtable_max_docs: usize::MAX, ..DbOptions::default() };
    let db = Db::create(&path, events(r#""retain": "1 day", "cluster_by": "usr","#), opts).unwrap();
    assert_eq!(db.schema().retain.as_ref().map(|r| (r.by.as_str(), r.seconds)), Some(("ts", 86_400)));
    let now = strata::timestamp::now();
    // Rows a day and a half old, rows that expire two seconds from now,
    // fresh rows and rows without a time.
    let old = now - 3 * DAY / 2;
    let soon = now - DAY + 2_000_000;
    write(&db, (0..100).map(|i| row(i, &usr(i), Some(old))));
    write(&db, (100..200).map(|i| row(i, &usr(i), Some(soon))));
    db.storage().flush().unwrap();
    // A flush writes no expired row.
    assert_eq!(count(&db, "SELECT COUNT(*) FROM events"), 100);
    write(&db, (200..300).map(|i| row(i, &usr(i), Some(now))));
    write(&db, (300..310).map(|i| row(i, &usr(i), None)));
    db.storage().flush().unwrap();
    assert_eq!(db.snapshot().segments().len(), 2);
    // The time is not changed in place.
    let mut t = db.storage().begin().unwrap();
    let e = t.set(&Value::Int(150), "ts", Value::Timestamp(now)).unwrap_err();
    assert!(e.to_string().contains("retain_by"), "{e}");
    drop(t);

    // Once the first segment's rows expire, a merge drops the segment whole
    // and leaves the other, whose rows are all kept.
    std::thread::sleep(Duration::from_millis(2500));
    let stats = db.storage().merge(&MergePolicy::default()).unwrap();
    assert_eq!((stats.merges, stats.docs_expired, stats.docs_merged), (1, 100, 0), "{stats:?}");
    assert_eq!(db.snapshot().segments().len(), 1);
    assert_eq!(ids(&db, "SELECT id FROM events WHERE id < 200"), Vec::<i64>::new());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM events"), 110);
    // Nothing more to do.
    assert_eq!(db.storage().merge(&MergePolicy::default()).unwrap().merges, 0);
}

#[test]
fn the_merger_drops_an_expired_segment_by_itself() {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { memtable_max_docs: usize::MAX, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("m.strata"), events(r#""retain": "1 day","#), opts).unwrap();
    let soon = strata::timestamp::now() - DAY + 1_000_000;
    write(&db, (0..50).map(|i| row(i, &usr(i), Some(soon))));
    db.storage().flush().unwrap();
    write(&db, (50..60).map(|i| row(i, &usr(i), Some(strata::timestamp::now()))));
    db.storage().flush().unwrap();
    db.spawn_merger(MergerOptions::default());
    let t0 = Instant::now();
    while count(&db, "SELECT COUNT(*) FROM events") > 10 {
        assert!(t0.elapsed() < Duration::from_secs(30), "the expired segment is still there");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(db.snapshot().segments().len(), 1);
}

#[test]
fn options_are_checked() {
    for (options, want) in [
        (r#""cluster_by": "nope","#, "not a field"),
        (r#""cluster_by": 3,"#, "a field name"),
        (r#""retain": "400 days", "retain_by": "n","#, "TIMESTAMP"),
        (r#""retain": "400 parsecs","#, "a whole number and a unit"),
        (r#""retain": "0 days","#, "a whole number and a unit"),
        (r#""retain_by": "ts","#, "give retain too"),
    ] {
        let e = Schema::from_json(&format!(
            r#"{{"table": "events", "key": "id", {options} "fields": [{{"name": "id", "type": "int"}}, {{"name": "n", "type": "int"}}, {{"name": "ts", "type": "timestamp"}}]}}"#
        ))
        .unwrap_err();
        assert!(e.to_string().contains(want), "{options}: {e}");
    }
    let e = Schema::from_json(r#"{"table": "t", "key": "id", "cluster_by": "body", "fields": [{"name": "id", "type": "int"}, {"name": "body", "type": "text"}]}"#).unwrap_err();
    assert!(e.to_string().contains("KEYWORD, INT or TIMESTAMP"), "{e}");
    let e = Schema::from_json(r#"{"table": "t", "key": "id", "retain": "1 day", "fields": [{"name": "id", "type": "int"}]}"#).unwrap_err();
    assert!(e.to_string().contains("name it with retain_by (t has 0)"), "{e}");
    for (period, seconds) in [("400 days", 400 * 86_400), ("12h", 43_200), ("1 week", 604_800), ("90 s", 90)] {
        assert_eq!(strata::schema::Retain::parse_period(period).unwrap(), seconds, "{period}");
    }
    // The long form round-trips.
    let s = events(r#""cluster_by": "usr", "retain": "2 weeks","#);
    let back = Schema::from_json(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back, s);
    assert_eq!(Schema::from_json_value(&s.to_short_json()).unwrap(), s);
}
