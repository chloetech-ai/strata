//! Field ids: a database names each field on disk by its id, not by its
//! position or its name. These tables give their fields ids that are not
//! their positions, so a reader that went by position would read the
//! wrong field: rows, filters, searches, values set in place, merges and
//! opening again must all go by id, in both layouts. A schema whose ids
//! clash is refused, and a table without ids gets them.

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema, NO_ID};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::Value;

/// `docs`, its fields given ids 7, 3, 12, 0 and 5, and next id 20, as a
/// table that had fields added and dropped would have.
fn docs() -> Schema {
    let fields = [("id", FieldType::Keyword, 7), ("n", FieldType::Int, 3), ("body", FieldType::Text { positions: true }, 12), ("tag", FieldType::Keyword, 0), ("score", FieldType::Float, 5)];
    let fields = fields
        .into_iter()
        .map(|(name, ty, id)| {
            let mut f = Field::new(name, ty);
            f.id = id;
            f
        })
        .collect();
    let mut s = Schema::new("docs", fields, Some("id")).unwrap();
    assert_eq!(s.next_field_id, 13);
    s.next_field_id = 20;
    s
}

fn options(layout: Layout) -> DbOptions {
    DbOptions { layout, sync: false, ..DbOptions::default() }
}

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => panic!("{sql}: expected rows"),
    }
}

/// Row `i` as written: `n` is `i`, the body names it, `tag` is `i % 3`
/// and `score` is `i / 2`.
fn row(i: i64, n: i64) -> Vec<Value> {
    vec![Value::Str(format!("d{i:03}")), Value::Int(n), Value::Str(format!("document {i} about cats {}", ["red", "green", "blue"][i as usize % 3])), Value::Str(format!("t{}", i % 3)), Value::Float(i as f64 / 2.0)]
}

/// What `db` holds: rows 0..40 but the deleted, `n` as set.
fn check(db: &Db, n: &dyn Fn(i64) -> Option<i64>, what: &str) {
    let all: Vec<Vec<Value>> = (0..40).filter_map(|i| n(i).map(|n| row(i, n))).collect();
    assert_eq!(rows(db, "SELECT id, n, body, tag, score FROM docs ORDER BY id LIMIT 100"), all, "{what}: every field");
    let ids = |f: &dyn Fn(&Vec<Value>) -> bool| all.iter().filter(|r| f(r)).map(|r| vec![r[0].clone()]).collect::<Vec<_>>();
    assert_eq!(rows(db, "SELECT id FROM docs WHERE tag = 't1' ORDER BY id LIMIT 100"), ids(&|r| r[3] == Value::Str("t1".into())), "{what}: keyword filter");
    assert_eq!(rows(db, "SELECT id FROM docs WHERE n >= 100 ORDER BY id LIMIT 100"), ids(&|r| matches!(r[1], Value::Int(n) if n >= 100)), "{what}: numeric filter");
    assert_eq!(rows(db, "SELECT id FROM docs WHERE score < 3 ORDER BY id LIMIT 100"), ids(&|r| matches!(r[4], Value::Float(s) if s < 3.0)), "{what}: float filter");
    let green = ids(&|r| matches!(&r[2], Value::Str(b) if b.ends_with("green")));
    assert_eq!(rows(db, "SELECT id FROM docs WHERE MATCH(body, 'green') ORDER BY id LIMIT 100"), green, "{what}: search");
    assert_eq!(rows(db, "SELECT id FROM docs WHERE MATCH(body, '\"cats green\"') ORDER BY id LIMIT 100"), green, "{what}: phrase");
}

#[test]
fn fields_are_read_by_id_not_position() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(if layout == Layout::Directory { "db.dir" } else { "db.strata" });
        let what = format!("{layout:?}");
        let db = Db::create(&path, docs(), options(layout)).unwrap();
        let stored = db.storage().table("docs").unwrap();
        assert_eq!(stored.fields.iter().map(|f| f.id).collect::<Vec<_>>(), [7, 3, 12, 0, 5], "{what}");
        assert_eq!(stored.next_field_id, 20, "{what}");
        {
            let mut tx = db.storage().begin().unwrap();
            for i in 0..30 {
                tx.insert(row(i, i)).unwrap();
            }
            tx.commit().unwrap();
        }
        db.storage().flush().unwrap();
        // Sections are named by id: f12 is body, and no section names a
        // field by its name.
        let snap = db.snapshot();
        let names: Vec<String> = snap.segments().iter().flat_map(|s| s.file.section_sizes().into_iter().map(|(k, _)| k)).collect();
        assert!(names.iter().any(|k| k.starts_with("text.f12.")), "{what}: {names:?}");
        assert!(!names.iter().any(|k| k.contains("body") || k.contains(".n.") || k.contains("tag")), "{what}: {names:?}");
        assert!(names.iter().any(|k| strata::segment::label(snap.schema(), k).starts_with("text.body.")), "{what}: {names:?}");
        drop(snap);

        let mut n: Vec<Option<i64>> = (0..40).map(|i| (i < 30).then_some(i)).collect();
        check(&db, &|i| n[i as usize], &what);
        // Values set in place, deletes, and rows left in the WAL.
        run(&db, "UPDATE docs SET n = 100 WHERE tag = 't2'");
        run(&db, "DELETE FROM docs WHERE id = 'd004'");
        for i in (0..30).filter(|i| i % 3 == 2) {
            n[i] = Some(100);
        }
        n[4] = None;
        check(&db, &|i| n[i as usize], &what);
        db.storage().flush().unwrap();
        {
            let mut tx = db.storage().begin().unwrap();
            for i in 30..40 {
                tx.insert(row(i, i)).unwrap();
                n[i as usize] = Some(i);
            }
            tx.commit().unwrap();
        }
        check(&db, &|i| n[i as usize], &what);
        drop(db);

        let db = Db::open(&path, options(layout)).unwrap();
        check(&db, &|i| n[i as usize], &format!("{what}, opened again"));
        db.storage().flush().unwrap();
        let stats = db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
        assert!(stats.docs_merged > 0, "{what}: {stats:?}");
        db.storage().check().unwrap();
        check(&db, &|i| n[i as usize], &format!("{what}, merged"));
        drop(db);
        let db = Db::open(&path, options(layout)).unwrap();
        check(&db, &|i| n[i as usize], &format!("{what}, merged and opened again"));
    }
}

#[test]
fn ids_that_clash_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = docs();
    s.fields[1].id = 7;
    let e = Db::create(&dir.path().join("a.strata"), s.clone(), DbOptions::default()).err().expect("two fields with id 7").to_string();
    assert!(e.contains("the same id"), "{e}");
    s.fields[1].id = 20;
    let e = Db::create(&dir.path().join("b.strata"), s.clone(), DbOptions::default()).err().expect("an id past the next").to_string();
    assert!(e.contains("next field id"), "{e}");
    s.fields[1].id = NO_ID;
    let e = Db::create(&dir.path().join("c.strata"), s, DbOptions::default()).err().expect("a field without an id").to_string();
    assert!(e.contains("next field id"), "{e}");
}

/// A schema without ids (as a database from before field ids holds it)
/// gets them, from 0, when it makes a database or a table.
#[test]
fn a_table_without_ids_gets_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("db.strata"), docs().without_ids(), DbOptions::default()).unwrap();
    let mut more = docs().without_ids();
    more.table = "more".into();
    db.create_table(more).unwrap();
    for t in ["docs", "more"] {
        let s = db.storage().table(t).unwrap();
        assert_eq!(s.fields.iter().map(|f| f.id).collect::<Vec<_>>(), [0, 1, 2, 3, 4], "{t}");
        assert_eq!(s.next_field_id, 5, "{t}");
    }
    drop(db);
    assert!(!strata::upgrade::needs_upgrade(&dir.path().join("db.strata")).unwrap());
}
