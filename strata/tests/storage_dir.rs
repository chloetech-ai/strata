//! The directory layout: the same database behaviour as the single file,
//! one file per segment, and segments that move between databases.

mod storage_util;

use std::collections::BTreeSet;
use std::sync::Arc;

use storage_util::*;
use strata::data;
use strata::db::Db;
use strata::query::Params;
use strata::storage::{Database, DbOptions, Layout, MergePolicy};
use strata::{Value, VectorIndexKind};

fn files(dir: &std::path::Path) -> BTreeSet<String> {
    std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
}

#[test]
fn directory_layout_writes_reads_and_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("db.dir");
    let src = Arc::new(MapSource::default());
    let o = DbOptions { memtable_max_docs: 50, segment_max_docs: 40, layout: Layout::Directory, ..opts(&src) };
    let db = Database::create(&path, schema(), o.clone()).unwrap();
    for i in 0..200u64 {
        let mut t = db.begin().unwrap();
        let d = doc(i, 1);
        src.put(&d);
        t.insert(d).unwrap();
        t.commit().unwrap();
    }
    db.flush().unwrap();
    let mut t = db.begin().unwrap();
    for i in (0..200u64).step_by(3) {
        t.delete(&Value::Str(key(i))).unwrap();
    }
    t.commit().unwrap();
    db.flush().unwrap();
    let names = files(&path);
    assert!(names.iter().any(|n| n.starts_with("seg-")), "{names:?}");
    assert!(names.iter().any(|n| n.starts_with("del-")), "{names:?}");
    assert_eq!(names.iter().filter(|n| n.starts_with("MANIFEST-")).count(), 1, "old manifests are removed: {names:?}");
    db.check().unwrap();

    let before = live_keys(&db.snapshot());
    let snap = db.snapshot();
    db.merge(&MergePolicy { merge_factor: 2, floor_docs: 10, ..MergePolicy::default() }).unwrap();
    // A snapshot taken before the merge still reads segments whose files are gone.
    assert_eq!(live_keys(&snap), before);
    drop(snap);
    assert_eq!(live_keys(&db.snapshot()), before);
    db.check().unwrap();

    // Uncommitted leftovers are removed when the database opens.
    std::fs::write(path.join("seg-9999999999.strata"), b"half written").unwrap();
    drop(db);
    let db = Database::open(&path, o).unwrap();
    assert!(!path.join("seg-9999999999.strata").exists());
    assert_eq!(live_keys(&db.snapshot()), before);
    db.check().unwrap();
    assert_eq!(db.vacuum().unwrap(), 0);
    let stats = db.file_stats();
    assert!(stats.file_bytes > 0 && stats.segments.iter().all(|s| s.bytes > 0));
}

#[test]
fn both_layouts_answer_alike_and_segments_move_between_them() {
    let tmp = tempfile::tempdir().unwrap();
    let docs = data::synthetic(1200, 8, 4);
    let schema = data::beir_schema(8, VectorIndexKind::Hnsw { m: 8, ef_construction: 40 });
    let mk = |name: &str, layout| Db::create(&tmp.path().join(name), schema.clone(), DbOptions { segment_max_docs: 400, layout, ..DbOptions::default() }).unwrap();
    let file_db = mk("one.strata", Layout::SingleFile);
    let dir_db = mk("many.dir", Layout::Directory);
    file_db.load(docs[..800].iter().cloned()).unwrap();
    dir_db.load(docs[..800].iter().cloned()).unwrap();
    let params = Params::new().set("q", docs[5][data::col::EMB].clone()).set("t", Value::Str(docs[5][data::col::TITLE].as_str().unwrap().into()));
    let sqls = [
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10",
        "SELECT id, score() FROM docs WHERE year > 2000 ORDER BY 0.5 * minmax(bm25(body, :t)) + 0.5 * minmax(similarity(emb, :q, strategy => 'flat')) DESC LIMIT 10",
        "SELECT category, COUNT(*) FROM docs GROUP BY category ORDER BY COUNT(*) DESC LIMIT 5",
    ];
    for sql in sqls {
        assert_eq!(file_db.query(sql, &params).unwrap().rows, dir_db.query(sql, &params).unwrap().rows, "{sql}");
    }

    // Build the remaining documents offline, ship the segment, attach it
    // to both databases.
    let built = tmp.path().join("built.strata-seg");
    Db::write_segment(&schema, docs[800..].to_vec(), &Default::default(), &built).unwrap();
    let a = file_db.attach_segments(std::slice::from_ref(&built)).unwrap();
    let b = dir_db.attach_segments(std::slice::from_ref(&built)).unwrap();
    assert_eq!((a.len(), b.len()), (1, 1));
    let count = "SELECT COUNT(*) FROM docs";
    assert_eq!(file_db.query(count, &Params::new()).unwrap().rows[0][0], Value::Int(1200));
    assert_eq!(dir_db.query(count, &Params::new()).unwrap().rows[0][0], Value::Int(1200));
    for sql in sqls {
        assert_eq!(file_db.query(sql, &params).unwrap().rows, dir_db.query(sql, &params).unwrap().rows, "{sql}");
    }
    // Attaching the same keys again is refused.
    assert!(dir_db.attach_segments(std::slice::from_ref(&built)).is_err());

    // Export from the directory database into a third, single-file one.
    let seg = dir_db.snapshot().segments()[0].id;
    let out = tmp.path().join("exported.strata-seg");
    dir_db.storage().export_segment(seg, &out).unwrap();
    let third = mk("third.strata", Layout::SingleFile);
    third.attach_segments(&[out]).unwrap();
    assert_eq!(third.query(count, &Params::new()).unwrap().rows[0][0], Value::Int(400));
    file_db.storage().check().unwrap();
    dir_db.storage().check().unwrap();
    third.storage().check().unwrap();

    // A segment with deletes cannot be exported.
    let mut t = dir_db.storage().begin().unwrap();
    t.delete(&docs[0][0]).unwrap();
    t.commit().unwrap();
    assert!(dir_db.storage().export_segment(seg, &tmp.path().join("x")).is_err());
}
