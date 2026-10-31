//! Multi-vector fields (`VECTOR(d)[]`): a row holds any number of vectors
//! (a page's patches, a song's frames), and `maxsim(field, vectors)` ranks
//! rows by the sum over the query vectors of each one's best match in the
//! row. With every vector searched and every candidate row scored it equals
//! a brute-force scan; by default it searches binary codes and scores with
//! SQ8 codes, close to the float32 answer. Rows survive merges, updates
//! and shards; similar vectors pool at write; the manifest names the
//! feature.

use std::path::Path;
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::cluster;
use strata::db::{Db, SqlResult};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::schema::{Field, FieldType, Schema};
use strata::storage::merge::MergePolicy;
use strata::storage::{DbOptions, Layout};
use strata::value::{Document, Value};
use strata::vector::kernels::dot_f32;
use strata::VectorIndexKind;

const DIM: usize = 16;
const TOPICS: usize = 24;
/// Every vector searched and every row scored: the exact answer.
const EXACT: &str = "depth => 1000000, candidates => 1000000";

fn schema(index: VectorIndexKind, keep_f32: bool) -> Schema {
    let mut patches = Field::new("patches", FieldType::Vector { dim: DIM, index }).multi();
    if keep_f32 {
        patches = patches.keep_f32();
    }
    let fields = vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Text { positions: true }), Field::new("cat", FieldType::Keyword), Field::new("year", FieldType::Int), patches];
    Schema::new("pages", fields, Some("id")).unwrap()
}

fn unit(v: Vec<f32>) -> Arc<[f32]> {
    let n = dot_f32(&v, &v).sqrt().max(1e-6);
    v.into_iter().map(|x| x / n).collect::<Vec<f32>>().into()
}

/// Rows of 0 to 12 vectors, each near one of its row's two topics.
fn pages(n: usize, seed: u64) -> Vec<(Document, Vec<Arc<[f32]>>)> {
    let mut rng = StdRng::seed_from_u64(seed);
    let topics: Vec<Vec<f32>> = (0..TOPICS).map(|_| (0..DIM).map(|_| rng.gen_range(-1.0..1.0)).collect()).collect();
    (0..n)
        .map(|i| {
            let ts = [rng.gen_range(0..TOPICS), rng.gen_range(0..TOPICS)];
            let k = if i % 17 == 0 { 0 } else { rng.gen_range(1..=12) };
            let vs: Vec<Arc<[f32]>> = (0..k).map(|_| unit(topics[ts[rng.gen_range(0..2)]].iter().map(|x| x + rng.gen_range(-0.4..0.4)).collect())).collect();
            let doc = vec![
                Value::Str(format!("p{i:05}")),
                Value::Str(format!("page {} about topic t{} and t{}", i, ts[0], ts[1])),
                Value::Str(format!("c{}", i % 5)),
                Value::Int(2000 + (i % 25) as i64),
                if vs.is_empty() { Value::Null } else { Value::List(vs.iter().map(|v| Value::Vector(v.clone())).collect()) },
            ];
            (doc, vs)
        })
        .collect()
}

fn open(dir: &Path, name: &str, docs: &[(Document, Vec<Arc<[f32]>>)], schema: Schema) -> Db {
    let opts = DbOptions { segment_max_docs: 300, sync: false, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), schema, opts).unwrap();
    db.load(docs.iter().map(|d| d.0.clone())).unwrap();
    db
}

/// A row's maxsim against `qs`, summed in the order the executor sums.
fn maxsim(vs: &[Arc<[f32]>], qs: &[Arc<[f32]>]) -> f32 {
    qs.iter().map(|q| vs.iter().map(|v| dot_f32(v, q)).fold(f32::NEG_INFINITY, f32::max)).sum()
}

fn brute(docs: &[(Document, Vec<Arc<[f32]>>)], qs: &[Arc<[f32]>], keep: impl Fn(&Document) -> bool, k: usize) -> Vec<(String, f64)> {
    let mut all: Vec<(String, f64)> =
        docs.iter().filter(|(d, vs)| !vs.is_empty() && keep(d)).map(|(d, vs)| (d[0].as_str().unwrap().to_string(), maxsim(vs, qs) as f64)).collect();
    all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    all.truncate(k);
    all
}

/// Query vectors: a few of one row's vectors, moved a little.
fn queries(docs: &[(Document, Vec<Arc<[f32]>>)], rng: &mut StdRng, n: usize) -> Vec<Arc<[f32]>> {
    let (_, vs) = loop {
        let d = &docs[rng.gen_range(0..docs.len())];
        if !d.1.is_empty() {
            break d;
        }
    };
    (0..n).map(|_| unit(vs[rng.gen_range(0..vs.len())].iter().map(|x| x + rng.gen_range(-0.2..0.2)).collect())).collect()
}

fn list(qs: &[Arc<[f32]>]) -> Value {
    Value::List(qs.iter().map(|q| Value::Vector(q.clone())).collect())
}

fn ranked(rs: &ResultSet) -> Vec<(String, f64)> {
    rs.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect()
}

fn rows(db: &Db, sql: &str, p: &Params) -> Vec<Vec<Value>> {
    match db.sql(sql, p).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn recall(got: &[(String, f64)], want: &[(String, f64)]) -> usize {
    want.iter().filter(|w| got.iter().any(|g| g.0 == w.0)).count()
}

#[test]
fn maxsim_over_rows_equals_brute_force() {
    let docs = pages(1500, 1);
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(2);
    let filters: [(&str, &dyn Fn(&Document) -> bool); 3] =
        [("", &|_| true), ("WHERE year >= 2012", &|d| d[3].as_i64().unwrap() >= 2012), ("WHERE cat IN ('c1', 'c3')", &|d| ["c1", "c3"].contains(&d[2].as_str().unwrap()))];
    for index in [VectorIndexKind::Flat, strata::schema::DEFAULT_HNSW] {
        let name = if index == VectorIndexKind::Flat { "flat" } else { "hnsw" };
        let db = open(dir.path(), &format!("{name}.strata"), &docs, schema(index, true));
        assert!(db.snapshot().segments().len() > 1, "several segments");
        for n in [1, 3, 8, 32] {
            for (filter, keep) in &filters {
                for k in [1, 10, 40] {
                    let qs = queries(&docs, &mut rng, n);
                    let sql = format!("SELECT id, score() FROM pages {filter} ORDER BY maxsim(patches, :qs, {EXACT}) DESC LIMIT {k}");
                    let got = ranked(&db.query(&sql, &Params::new().set("qs", list(&qs))).unwrap());
                    let want = brute(&docs, &qs, keep, k);
                    assert_eq!(got, want, "{name}: {sql} with {n} vectors");
                }
            }
        }
        // The default depth and candidate cap find nearly all of it.
        let (mut hit, mut total) = (0, 0);
        for _ in 0..30 {
            let qs = queries(&docs, &mut rng, 8);
            let got = ranked(&db.query("SELECT id, score() FROM pages ORDER BY maxsim(patches, :qs) DESC LIMIT 10", &Params::new().set("qs", list(&qs))).unwrap());
            let want = brute(&docs, &qs, |_| true, 10);
            hit += recall(&got, &want);
            total += want.len();
        }
        assert!(hit as f64 >= 0.95 * total as f64, "{name}: recall@10 {hit}/{total}");
    }
}

#[test]
fn sq8_scores_without_float32_are_close() {
    let docs = pages(1500, 3);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "sq8.strata", &docs, schema(strata::schema::DEFAULT_HNSW, false));
    let f32db = open(dir.path(), "f32.strata", &docs, schema(strata::schema::DEFAULT_HNSW, true));
    // SQ8 codes to score with instead of float32.
    let sections = |db: &Db| db.snapshot().segments()[0].file.section_sizes().into_iter().map(|s| s.0).filter(|n| n.starts_with("vec.patches")).collect::<Vec<_>>();
    assert!(sections(&db).iter().any(|n| n.ends_with(".rescore")) && !sections(&db).iter().any(|n| n.ends_with(".f32")), "{:?}", sections(&db));
    assert!(sections(&f32db).iter().any(|n| n.ends_with(".f32")) && !sections(&f32db).iter().any(|n| n.ends_with(".rescore")), "{:?}", sections(&f32db));
    let mut rng = StdRng::seed_from_u64(4);
    let (mut hit, mut total) = (0, 0);
    for _ in 0..30 {
        let qs = queries(&docs, &mut rng, 6);
        let p = Params::new().set("qs", list(&qs));
        let exact = ranked(&db.query(&format!("SELECT id, score() FROM pages ORDER BY maxsim(patches, :qs, {EXACT}) DESC LIMIT 10"), &p).unwrap());
        let want = brute(&docs, &qs, |_| true, 10);
        // Each SQ8 score is within a few hundredths of the float32 one.
        for (id, s) in &exact {
            let (_, vs) = docs.iter().find(|d| d.0[0].as_str() == Some(id)).unwrap();
            let f = maxsim(vs, &qs) as f64;
            assert!((s - f).abs() < 0.02 * qs.len() as f64, "{id}: {s} vs {f}");
        }
        hit += recall(&exact, &want);
        let got = ranked(&db.query("SELECT id, score() FROM pages ORDER BY maxsim(patches, :qs) DESC LIMIT 10", &p).unwrap());
        hit += recall(&got, &want);
        total += 2 * want.len();
    }
    assert!(hit as f64 >= 0.9 * total as f64, "recall@10 {hit}/{total}");
}

/// The coordinator's rounds over `shards`, as `cluster_split.rs` runs them.
fn sharded(shards: &[&Db], sql: &str, params: &Params) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let select = bind(sql, snaps[0].schema(), params).unwrap();
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
    let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap()).collect();
    cluster::merge(&select, answers).unwrap()
}

#[test]
fn shards_answer_like_one_database() {
    let docs = pages(1200, 5);
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs, schema(strata::schema::DEFAULT_HNSW, true));
    let a = open(dir.path(), "a.strata", &docs[..600], schema(strata::schema::DEFAULT_HNSW, true));
    let b = open(dir.path(), "b.strata", &docs[600..], schema(strata::schema::DEFAULT_HNSW, true));
    let mut rng = StdRng::seed_from_u64(6);
    let params = Params::new().set("qs", list(&queries(&docs, &mut rng, 5))).set("t", Value::Str("topic t3".into()));
    for sql in [
        format!("SELECT id, score() FROM pages ORDER BY maxsim(patches, :qs, {EXACT}) DESC LIMIT 10"),
        format!("SELECT id, score() FROM pages WHERE year > 2010 ORDER BY maxsim(patches, :qs, {EXACT}) DESC LIMIT 25"),
        format!("SELECT id, score() FROM pages ORDER BY maxsim(patches, :qs, {EXACT}) DESC LIMIT 5 OFFSET 7"),
        format!("SELECT id, score() FROM pages ORDER BY rrf(maxsim(patches, :qs, {EXACT}), bm25(title, :t)) DESC LIMIT 10"),
        format!("SELECT id, score() FROM pages ORDER BY maxsim(patches, :qs, {EXACT}) * year DESC LIMIT 10"),
    ] {
        let want = whole.query(&sql, &params).unwrap();
        let got = sharded(&[&a, &b], &sql, &params);
        assert!(!want.rows.is_empty(), "{sql}");
        assert_eq!(want.rows.len(), got.rows.len(), "{sql}");
        for (w, g) in want.rows.iter().zip(&got.rows) {
            assert_eq!(w[0], g[0], "{sql}");
            assert!((w[1].as_f64().unwrap() - g[1].as_f64().unwrap()).abs() < 1e-9, "{sql}: {w:?} vs {g:?}");
        }
    }
}

#[test]
fn rows_keep_their_vectors_through_merges_updates_and_deletes() {
    let docs = pages(1200, 7);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "m.strata", &docs, schema(strata::schema::DEFAULT_HNSW, true));
    let mut rng = StdRng::seed_from_u64(8);
    let qs = queries(&docs, &mut rng, 4);
    let p = Params::new().set("qs", list(&qs));
    let sql = format!("SELECT id, score() FROM pages ORDER BY maxsim(patches, :qs, {EXACT}) DESC LIMIT 20");
    let before = ranked(&db.query(&sql, &p).unwrap());
    assert_eq!(before, brute(&docs, &qs, |_| true, 20));

    // A row's vectors come back as a list, in their order; a row without
    // any as NULL.
    let stored = |db: &Db, id: &str| rows(db, &format!("SELECT patches FROM pages WHERE id = '{id}'"), &Params::new()).remove(0).remove(0);
    assert_eq!(stored(&db, "p00001"), docs[1].0[4]);
    assert_eq!(stored(&db, "p00000"), Value::Null);

    let segs = db.snapshot().segments().len();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
    assert!(db.snapshot().segments().len() < segs, "merged");
    assert_eq!(ranked(&db.query(&sql, &p).unwrap()), before);
    assert_eq!(stored(&db, "p00001"), docs[1].0[4]);

    // Replace the best row's vectors with the query's own: it scores the
    // most a row can. Delete the second.
    let (best, second) = (before[0].0.clone(), before[1].0.clone());
    db.sql(&format!("UPDATE pages SET patches = :qs WHERE id = '{}'", before[5].0), &p).unwrap();
    db.sql(&format!("DELETE FROM pages WHERE id = '{second}'"), &Params::new()).unwrap();
    let after = ranked(&db.query(&sql, &p).unwrap());
    assert_eq!(after[0].0, before[5].0);
    assert_eq!(after[0].1, maxsim(&qs, &qs) as f64);
    assert!(after.iter().all(|r| r.0 != second));
    assert!(after.iter().any(|r| r.0 == best));
    db.sql(&format!("UPDATE pages SET patches = NULL WHERE id = '{best}'"), &Params::new()).unwrap();
    assert!(ranked(&db.query(&sql, &p).unwrap()).iter().all(|r| r.0 != best), "a row without vectors does not rank");
    assert_eq!(stored(&db, &best), Value::Null);
}

fn sql_db(path: &Path, create: &str) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, DbOptions { layout: Layout::Directory, sync: false, ..DbOptions::default() }).unwrap();
    db.sql(create, &Params::new()).unwrap();
    db
}

fn vectors(v: &Value) -> Vec<Vec<f32>> {
    match v {
        Value::List(xs) => xs.iter().map(|x| if let Value::Vector(v) = x { v.to_vec() } else { panic!("{x:?}") }).collect(),
        v => panic!("not a list of vectors: {v:?}"),
    }
}

#[test]
fn sql_writes_and_reads_multi_vector_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p");
    let db = sql_db(&path, "CREATE TABLE p (id KEYWORD PRIMARY KEY, patches VECTOR(3)[] OPTIONS(index = 'flat', keep_f32 = true))");
    let none = Params::new();
    db.sql("INSERT INTO p (id, patches) VALUES ('a', ARRAY[ARRAY[1.0, 0.0, 0.0], ARRAY[0.0, 1.0, 0.0]])", &none).unwrap();
    db.sql("INSERT INTO p (id, patches) VALUES ('b', '[[0.0, 0.0, 1.0]]')", &none).unwrap();
    db.sql("INSERT INTO p (id, patches) VALUES ('c', :v)", &Params::new().set("v", Value::Vector(vec![0.5, 0.5, 0.0].into()))).unwrap();
    db.sql("INSERT INTO p (id) VALUES ('d')", &none).unwrap();
    let mut got = rows(&db, "SELECT id, patches FROM p", &none);
    got.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
    assert_eq!(vectors(&got[0][1]), vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]]);
    assert_eq!(vectors(&got[1][1]), vec![vec![0.0, 0.0, 1.0]]);
    assert_eq!(vectors(&got[2][1]), vec![vec![0.5, 0.5, 0.0]], "one vector is a list of one");
    assert_eq!(got[3][1], Value::Null);

    // Two query vectors: a matches both (1 + 1), c half of each (0.5 + 0.5).
    let qs = Params::new().set("qs", Value::List(vec![Value::Vector(vec![1.0, 0.0, 0.0].into()), Value::Vector(vec![0.0, 1.0, 0.0].into())]));
    let r = rows(&db, "SELECT id, score() FROM p ORDER BY maxsim(patches, :qs) DESC LIMIT 5", &qs);
    let r: Vec<(String, f64)> = r.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect();
    assert_eq!(r, vec![("a".to_string(), 2.0), ("c".to_string(), 1.0), ("b".to_string(), 0.0)]);

    // The manifest names the feature.
    let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join(cur["manifest"].as_str().unwrap())).unwrap()).unwrap();
    assert!(manifest["features"]["required"].as_array().unwrap().iter().any(|f| f == "multi_vector"), "{}", manifest["features"]);

    // Survives a reopen.
    drop(db);
    let db = Db::open(&path, DbOptions { layout: Layout::Directory, sync: false, ..DbOptions::default() }).unwrap();
    assert_eq!(rows(&db, "SELECT id, score() FROM p ORDER BY maxsim(patches, :qs) DESC LIMIT 1", &qs)[0][0], Value::Str("a".into()));
}

#[test]
fn similar_vectors_pool_at_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = sql_db(&dir.path().join("p"), "CREATE TABLE p (id KEYWORD PRIMARY KEY, patches VECTOR(2)[] OPTIONS(index = 'flat', keep_f32 = true, pool = 0.99))");
    // Three near copies of one direction and one other: two vectors stay.
    db.sql("INSERT INTO p (id, patches) VALUES ('a', ARRAY[ARRAY[1.0, 0.0], ARRAY[1.0, 0.01], ARRAY[1.0, -0.01], ARRAY[0.0, 1.0]])", &Params::new()).unwrap();
    let got = vectors(&rows(&db, "SELECT patches FROM p", &Params::new())[0][0]);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().any(|v| (v[0] - 1.0).abs() < 1e-3 && v[1].abs() < 1e-3), "{got:?}");
    assert!(got.contains(&vec![0.0, 1.0]), "{got:?}");
}

#[test]
fn multi_vector_fields_refuse_what_they_cannot_do() {
    let dir = tempfile::tempdir().unwrap();
    let db = sql_db(&dir.path().join("p"), "CREATE TABLE p (id KEYWORD PRIMARY KEY, title TEXT, patches VECTOR(3)[] OPTIONS(index = 'flat'), one VECTOR(3) OPTIONS(index = 'flat'))");
    let q = Params::new().set("q", Value::Vector(vec![1.0, 0.0, 0.0].into()));
    let e = |sql: &str, p: &Params| db.sql(sql, p).err().unwrap_or_else(|| panic!("{sql} is refused")).to_string();
    assert!(e("SELECT id FROM p ORDER BY similarity(patches, :q) DESC LIMIT 5", &q).contains("maxsim(patches, vectors)"));
    let msg = e("SELECT id FROM p WHERE REACHABLE(patches, ('a')) LIMIT 5", &q);
    assert!(msg.contains("multi-vector"), "{msg}");
    assert!(e("INSERT INTO p (id, patches) VALUES ('x', ARRAY[ARRAY[1.0, 0.0]])", &q).contains("3"));
    for (create, what) in [
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, v VECTOR(3)[] OPTIONS(pool = 1.5))", "pool"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, v VECTOR(3) OPTIONS(keep_f32 = true))", "keep_f32"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, body TEXT, v VECTOR[] OPTIONS(embed = 'body', model = 'hash:16'))", "embed"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, v VECTOR(3)[], sid KEYWORD[] OPTIONS(semantic_id = 'v', codebook = 'c'))", "multi-vector"),
    ] {
        let msg = e(create, &Params::new());
        assert!(msg.contains(what), "{create}: {msg}");
    }
}
