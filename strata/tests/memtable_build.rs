//! Memtable segments built with their own options (`DbOptions::mem_build`).

use strata::db::Db;
use strata::query::Params;
use strata::segment::build::BuildOptions;
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value, VectorIndexKind};

const DIM: usize = 16;

fn unit(i: u64) -> Vec<f32> {
    let v: Vec<f32> = (0..DIM as u64).map(|d| ((i * 2654435761 + d * 40503) % 1009) as f32 / 1009.0 - 0.5).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.into_iter().map(|x| x / n).collect()
}

fn top10(db: &Db, q: &[f32]) -> Vec<String> {
    let rs = db.query("SELECT id FROM t ORDER BY similarity(emb, :q) DESC LIMIT 10", &Params::new().set("q", Value::Vector(q.to_vec().into()))).unwrap();
    rs.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

/// With flat memtable segments, vector search over unflushed documents is
/// exact, and flushed segments still get the schema's graph index.
#[test]
fn flat_memtable_segments_search_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::new(
        "t",
        vec![Field::new("id", FieldType::Keyword), Field::new("emb", FieldType::Vector { dim: DIM, index: VectorIndexKind::Hnsw { m: 8, ef_construction: 40 } })],
        Some("id"),
    )
    .unwrap();
    let opts = DbOptions { mem_build: Some(BuildOptions::default().set("vector.index", "flat")), memtable_chunk_docs: 64, sync: false, ..Default::default() };
    let db = Db::create(&dir.path().join("m.strata"), schema, opts).unwrap();
    let n = 300u64;
    for t in 0..10 {
        let mut tx = db.storage().begin().unwrap();
        for i in t * n / 10..(t + 1) * n / 10 {
            tx.insert(vec![Value::Str(format!("d{i}")), Value::Vector(unit(i).into())]).unwrap();
        }
        tx.commit().unwrap();
    }
    let snap = db.snapshot();
    assert!(snap.segments().len() > 1 && snap.segments().iter().all(|s| s.in_memory));
    for qi in [7u64, 1234, 99_999] {
        let q = unit(qi);
        let mut exact: Vec<(f32, u64)> = (0..n).map(|i| (unit(i).iter().zip(&q).map(|(a, b)| a * b).sum(), i)).collect();
        exact.sort_by(|a, b| b.0.total_cmp(&a.0));
        let want: Vec<String> = exact.iter().take(10).map(|(_, i)| format!("d{i}")).collect();
        assert_eq!(top10(&db, &q), want);
    }
    db.storage().flush().unwrap();
    let snap = db.snapshot();
    assert!(snap.segments().iter().all(|s| !s.in_memory));
    assert_eq!(top10(&db, &unit(7)).len(), 10);
}
