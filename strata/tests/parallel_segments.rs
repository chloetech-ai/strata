//! Searching segments on several threads returns exactly what one thread
//! returns.

use strata::data;
use strata::db::Db;
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

#[test]
fn threads_do_not_change_answers() {
    let docs = data::synthetic(4000, 16, 11);
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { segment_max_docs: 300, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("p.strata"), data::beir_schema(16, VectorIndexKind::Hnsw { m: 12, ef_construction: 60 }), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    // Deletes give segments live filters.
    let mut t = db.storage().begin().unwrap();
    for i in (0..4000).step_by(7) {
        t.delete(&Value::Str(format!("d{i:07}"))).unwrap();
    }
    t.commit().unwrap();
    assert!(db.snapshot().segments().len() >= 10);
    let title = docs[9][data::col::TITLE].as_str().unwrap().to_string();
    let params = Params::new().set("q", docs[9][data::col::EMB].clone()).set("t", Value::Str(title));
    let queries = [
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 20",
        "SELECT id, score() FROM docs WHERE year BETWEEN 2000 AND 2010 ORDER BY bm25(body, :t) DESC LIMIT 20",
        "SELECT id, score() FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 20",
        "SELECT id, score() FROM docs WHERE category IN ('c00', 'c05') ORDER BY similarity(emb, :q) DESC LIMIT 20",
        "SELECT id, score() FROM docs WHERE rating < 1 ORDER BY similarity(emb, :q, strategy => 'postfilter') DESC LIMIT 20",
        "SELECT id, score() FROM docs ORDER BY 0.4 * minmax(bm25(body, :t)) + 0.6 * minmax(similarity(emb, :q)) DESC LIMIT 20",
        "SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 20",
    ];
    for sql in queries {
        let one = db.query_with(sql, &params, &ExecOptions::default()).unwrap();
        for threads in [2, 4, 8] {
            let many = db.query_with(sql, &params, &ExecOptions { threads, ..ExecOptions::default() }).unwrap();
            assert_eq!(one.rows, many.rows, "{sql} on {threads} threads");
            assert!(many.stats.plan.iter().any(|l| l.contains("threads")), "{sql}: {:?}", many.stats.plan);
        }
    }
}

#[test]
fn large_segments_build_ivf_instead_of_hnsw() {
    use strata::segment::build::BuildOptions;
    let docs = data::synthetic(900, 16, 2);
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { segment_max_docs: 600, build: BuildOptions::default().set("vector.hnsw.max_docs", 400), ..DbOptions::default() };
    let db = Db::create(&dir.path().join("i.strata"), data::beir_schema(16, VectorIndexKind::Hnsw { m: 12, ef_construction: 60 }), opts).unwrap();
    db.load(docs).unwrap();
    let snap = db.snapshot();
    let kinds: Vec<String> = db.readers(&snap).unwrap().iter().map(|r| r.vector(data::col::EMB).unwrap().unwrap().index_kind().to_string()).collect();
    assert_eq!(kinds, vec!["ivf", "hnsw"], "600 vectors exceed the bound, 300 do not");
}
