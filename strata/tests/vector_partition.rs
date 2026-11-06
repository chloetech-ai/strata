//! VECTOR fields partitioned by a column (`OPTIONS(partition_by = ..)`,
//! [`strata::schema::VectorPartition`]): each segment keeps a graph per
//! partition, and a search whose filter limits the column walks only the
//! graphs of the partitions it names. These tests check the answers against
//! an exact search, with deleted rows, NULL keys, merges, and KEYWORD keys.

use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::Value;

const DIM: usize = 16;
const CATS: [&str; 4] = ["a", "b", "c", "d"];

fn seed() -> Schema {
    Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap()
}

fn opts() -> DbOptions {
    DbOptions { sync: false, layout: Layout::SingleFile, ..DbOptions::default() }
}

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// A unit vector near one of 8 centres; its category follows the centre,
/// so a filter on the category is correlated with the vectors.
struct Row {
    id: usize,
    rating: Option<f64>,
    cat: &'static str,
    v: Vec<f32>,
}

fn rows(n: usize, seed: u64) -> Vec<Row> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let centres: Vec<Vec<f32>> = (0..8).map(|_| (0..DIM).map(|_| rng.gen_range(-1.0f32..1.0)).collect()).collect();
    (0..n)
        .map(|id| {
            let c = rng.gen_range(0..8);
            let mut v: Vec<f32> = centres[c].iter().map(|x| x + rng.gen_range(-0.5f32..0.5)).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= norm);
            // One row in 50 has no rating.
            let rating = (id % 50 != 7).then(|| (rng.gen_range(0.0..5.0f64) * 100.0).round() / 100.0);
            Row { id, rating, cat: CATS[c % 4], v }
        })
        .collect()
}

fn vec_sql(v: &[f32]) -> String {
    format!("[{}]", v.iter().map(|x| format!("{x}")).collect::<Vec<_>>().join(", "))
}

fn insert(db: &Db, rows: &[Row]) {
    for chunk in rows.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|r| format!("('r{}', {}, '{}', {})", r.id, r.rating.map_or("NULL".into(), |x| x.to_string()), r.cat, vec_sql(&r.v)))
            .collect();
        run(db, &format!("INSERT INTO t (id, rating, cat, emb) VALUES {}", values.join(", ")));
    }
}

fn create(db: &Db, by: &str) {
    let opts = match by {
        "rating" => "partition_by = 'rating', partition_bounds = '1, 2, 3, 4'",
        _ => "partition_by = 'cat'",
    };
    run(db, &format!("CREATE TABLE t (id KEYWORD PRIMARY KEY, rating FLOAT, cat KEYWORD, emb VECTOR({DIM}) OPTIONS(m = 8, ef_construction = 64, {opts}))"));
}

/// The ids and the plan of a top-10 under `filter` with similarity
/// options `extra`.
fn top(db: &Db, q: &[f32], filter: &str, extra: &str) -> (Vec<String>, Vec<String>) {
    let w = if filter.is_empty() { String::new() } else { format!("WHERE {filter}") };
    let sql = format!("SELECT id FROM t {w} ORDER BY similarity(emb, :q{extra}) DESC LIMIT 10");
    let p = Params::new().set("q", Value::Vector(q.to_vec().into()));
    let r = db.query(&sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"));
    (r.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect(), r.stats.plan)
}

/// Mean recall@10 of the partitions' answers against an exact search, and
/// whether every query's plan walked partitions.
fn recall(db: &Db, queries: &[Row], filter: &str) -> f64 {
    let mut sum = 0.0;
    for q in queries {
        let (got, plan) = top(db, &q.v, filter, ", strategy => 'partition'");
        assert!(plan.iter().any(|l| l.contains("vector partitions")), "{filter}: {plan:?}");
        let (want, _) = top(db, &q.v, filter, ", strategy => 'prefilter', rerank => 10000");
        sum += want.iter().filter(|w| got.contains(w)).count() as f64 / want.len().max(1) as f64;
        assert!(got.len() == want.len(), "{filter}: {} hits, exact {}", got.len(), want.len());
    }
    sum / queries.len() as f64
}

fn rating_of(data: &[Row], id: &str) -> Option<f64> {
    data[id[1..].parse::<usize>().unwrap()].rating
}

#[test]
fn rating_partitions_answer_range_filters() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("p.strata"), seed(), opts()).unwrap();
    create(&db, "rating");
    let data = rows(4000, 1);
    insert(&db, &data);
    db.storage().flush().unwrap();
    let queries = rows(20, 2);
    // Whole ranges, a part of one, two ranges, a range and another column.
    for f in ["rating >= 4", "rating >= 4.5", "rating < 1", "rating >= 2 AND rating < 4", "rating BETWEEN 1.5 AND 2.5", "rating >= 3 AND cat = 'a'", "rating > 4"] {
        let r = recall(&db, &queries, f);
        assert!(r >= 0.9, "{f}: recall {r}");
    }
    // Every hit passes the filter; rows without a rating are never in a partition.
    for q in &queries {
        let (got, _) = top(&db, &q.v, "rating >= 4.5", ", strategy => 'partition'");
        assert!(got.iter().all(|id| rating_of(&data, id).is_some_and(|x| x >= 4.5)), "{got:?}");
        let (got, plan) = top(&db, &q.v, "rating >= 4", ", strategy => 'partition'");
        assert!(got.iter().all(|id| rating_of(&data, id).is_some_and(|x| x >= 4.0)), "{got:?}");
        // A filter the partitions guarantee is not tested at all.
        assert!(plan.iter().any(|l| l.contains("vector partitions range 4") && l.contains("filter none")), "{plan:?}");
    }
    // The last range has no upper end: a filter past every rating walks it
    // and finds nothing.
    let (got, plan) = top(&db, &queries[0].v, "rating > 7", ", strategy => 'partition'");
    assert!(got.is_empty(), "{got:?}");
    assert!(plan.iter().any(|l| l.contains("vector partitions range 4") && l.contains("filter Probe")), "{plan:?}");
    // Without a condition on the key the field's graph answers.
    let (_, plan) = top(&db, &queries[0].v, "cat = 'a'", ", strategy => 'partition'");
    assert!(!plan.iter().any(|l| l.contains("vector partitions")), "{plan:?}");
}

#[test]
fn deleted_rows_and_merges_keep_partitions_right() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("p.strata"), seed(), opts()).unwrap();
    create(&db, "rating");
    let data = rows(3000, 3);
    for part in data.chunks(1000) {
        insert(&db, part);
        db.storage().flush().unwrap();
    }
    let queries = rows(10, 4);
    // Delete each query's best rated-4 rows: they must not come back.
    let mut gone = Vec::new();
    for q in &queries {
        let (got, _) = top(&db, &q.v, "rating >= 4", ", strategy => 'partition'");
        gone.extend(got.into_iter().take(3));
    }
    gone.sort();
    gone.dedup();
    let list: Vec<String> = gone.iter().map(|g| format!("'{g}'")).collect();
    run(&db, &format!("DELETE FROM t WHERE id IN ({})", list.join(", ")));
    for q in &queries {
        let (got, _) = top(&db, &q.v, "rating >= 4", ", strategy => 'partition'");
        assert!(got.iter().all(|g| !gone.contains(g)), "{got:?}");
    }
    assert!(recall(&db, &queries, "rating >= 4") >= 0.9);
    // Merged into one segment, the partitions are built again from the rows.
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 100_000, ..MergePolicy::default() }).unwrap();
    assert_eq!(strata::storage::manifest::read(&dir.path().join("p.strata")).unwrap().segments().count(), 1);
    assert!(recall(&db, &queries, "rating >= 4") >= 0.9);
    assert!(recall(&db, &queries, "rating < 2") >= 0.9);
    for q in &queries {
        let (got, _) = top(&db, &q.v, "rating >= 4", ", strategy => 'partition'");
        assert!(got.iter().all(|g| !gone.contains(g)), "{got:?}");
    }
    // And the database says it needs a version of Strata that keeps them.
    let features = strata::storage::manifest::read(&dir.path().join("p.strata")).unwrap().features();
    assert!(features.contains(&"vector_partition".to_string()), "{features:?}");
    // Unfiltered searches still find the rows without a rating.
    let unrated = data.iter().find(|r| r.rating.is_none()).unwrap();
    let (got, _) = top(&db, &unrated.v, "", "");
    assert_eq!(got.first().map(String::as_str), Some(format!("r{}", unrated.id).as_str()));
    // Opened again, the table keeps its option and its partitions.
    let before: Vec<Vec<String>> = queries.iter().map(|q| top(&db, &q.v, "rating >= 4", ", strategy => 'partition'").0).collect();
    drop(db);
    let db = Db::open(&dir.path().join("p.strata"), opts()).unwrap();
    let after: Vec<Vec<String>> = queries.iter().map(|q| top(&db, &q.v, "rating >= 4", ", strategy => 'partition'").0).collect();
    assert_eq!(before, after);
    let t = db.catalog().into_iter().find(|s| s.table == "t").unwrap();
    let sql = strata::query::ddl::create_sql(&t);
    assert!(sql.contains("partition_by = 'rating', partition_bounds = '1, 2, 3, 4'"), "{sql}");
}

#[test]
fn int_keys_and_bad_options() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("p.strata"), seed(), opts()).unwrap();
    run(&db, &format!("CREATE TABLE u (id KEYWORD PRIMARY KEY, year INT, emb VECTOR({DIM}) OPTIONS(m = 8, partition_by = 'year', partition_bounds = '2010, 2020'))"));
    let data = rows(2000, 9);
    for chunk in data.chunks(500) {
        let values: Vec<String> = chunk.iter().map(|r| format!("('r{}', {}, {})", r.id, 2000 + r.id % 30, vec_sql(&r.v))).collect();
        run(&db, &format!("INSERT INTO u (id, year, emb) VALUES {}", values.join(", ")));
    }
    db.storage().flush().unwrap();
    let p = Params::new().set("q", Value::Vector(data[3].v.clone().into()));
    let r = db.query("SELECT id, year FROM u WHERE year >= 2020 ORDER BY similarity(emb, :q, strategy => 'partition') DESC LIMIT 10", &p).unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(r.rows.iter().all(|row| row[1].as_i64().unwrap() >= 2020), "{:?}", r.rows);
    assert!(r.stats.plan.iter().any(|l| l.contains("vector partitions range 2") && l.contains("filter none")), "{:?}", r.stats.plan);
    let r = db.query("SELECT id FROM u WHERE year IN (2003, 2012) ORDER BY similarity(emb, :q, strategy => 'partition') DESC LIMIT 10", &p).unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(r.stats.plan.iter().any(|l| l.contains("vector partitions range 0, range 1") && l.contains("filter Probe")), "{:?}", r.stats.plan);
    // The options name a column the table can partition by.
    for bad in [
        format!("CREATE TABLE b1 (id KEYWORD PRIMARY KEY, y INT, emb VECTOR({DIM}) OPTIONS(partition_by = 'y'))"),
        format!("CREATE TABLE b2 (id KEYWORD PRIMARY KEY, y INT, emb VECTOR({DIM}) OPTIONS(partition_by = 'z', partition_bounds = '1'))"),
        format!("CREATE TABLE b3 (id KEYWORD PRIMARY KEY, y INT, emb VECTOR({DIM}) OPTIONS(partition_by = 'y', partition_bounds = '3, 1'))"),
        format!("CREATE TABLE b4 (id KEYWORD PRIMARY KEY, y INT, emb VECTOR({DIM}) OPTIONS(index = 'flat', partition_by = 'y', partition_bounds = '1'))"),
        format!("CREATE TABLE b5 (id KEYWORD PRIMARY KEY, c KEYWORD, emb VECTOR({DIM}) OPTIONS(partition_by = 'c', partition_bounds = '1'))"),
        format!("CREATE TABLE b6 (id KEYWORD PRIMARY KEY, t TEXT, emb VECTOR({DIM}) OPTIONS(partition_by = 't'))"),
    ] {
        assert!(db.sql(&bad, &Params::new()).is_err(), "{bad}");
    }
}

#[test]
fn keyword_partitions_answer_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("p.strata"), seed(), opts()).unwrap();
    create(&db, "cat");
    let data = rows(3000, 5);
    insert(&db, &data);
    db.storage().flush().unwrap();
    let queries = rows(20, 6);
    for f in ["cat = 'a'", "cat IN ('b', 'd')", "cat = 'c' AND rating >= 2"] {
        let r = recall(&db, &queries, f);
        assert!(r >= 0.9, "{f}: recall {r}");
    }
    let (got, plan) = top(&db, &queries[0].v, "cat = 'zzz'", ", strategy => 'partition'");
    assert!(got.is_empty() && plan.iter().any(|l| l.contains("none can hold a match")), "{got:?} {plan:?}");
}

#[test]
fn the_cost_model_weighs_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("p.strata"), seed(), opts()).unwrap();
    create(&db, "rating");
    insert(&db, &rows(3000, 7));
    db.storage().flush().unwrap();
    let q = rows(1, 8).remove(0);
    // Without a forced strategy the plan says what it weighed.
    let (_, plan) = top(&db, &q.v, "rating >= 4", "");
    assert!(plan.iter().any(|l| l.contains("vector partitions")), "{plan:?}");
    // A forced strategy other than partitions never walks them.
    let (_, plan) = top(&db, &q.v, "rating >= 4", ", strategy => 'postfilter'");
    assert!(!plan.iter().any(|l| l.contains("vector partitions")), "{plan:?}");
    // Nor does a query that turns them off, unless it asks for them.
    let off = ExecOptions { vector_partitions: false, ..ExecOptions::default() };
    let p = Params::new().set("q", Value::Vector(q.v.clone().into()));
    let r = db.query_with("SELECT id FROM t WHERE rating >= 4 ORDER BY similarity(emb, :q) DESC LIMIT 10", &p, &off).unwrap();
    assert!(!r.stats.plan.iter().any(|l| l.contains("vector partitions")), "{:?}", r.stats.plan);
    let r = db.query_with("SELECT id FROM t WHERE rating >= 4 ORDER BY similarity(emb, :q, strategy => 'partition') DESC LIMIT 10", &p, &off).unwrap();
    assert!(r.stats.plan.iter().any(|l| l.contains("vector partitions range 4")), "{:?}", r.stats.plan);
}

#[test]
fn merges_start_partition_graphs_only_from_the_same_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("p.strata"), seed(), opts()).unwrap();
    create(&db, "rating");
    let mut data = rows(3000, 5);
    for part in data.chunks(1000) {
        insert(&db, part);
        db.storage().flush().unwrap();
    }
    // Rows of the second segment move to another partition after it was
    // built: a merge must not start their partitions' graphs from it.
    let moved: Vec<usize> = (1000..2000).step_by(3).filter(|&i| data[i].rating.is_some()).collect();
    for chunk in moved.chunks(200) {
        let ids: Vec<String> = chunk.iter().map(|i| format!("'r{i}'")).collect();
        run(&db, &format!("UPDATE t SET rating = 4.5 WHERE id IN ({})", ids.join(", ")));
    }
    for &i in &moved {
        data[i].rating = Some(4.5);
    }
    db.storage().merge(&MergePolicy { merge_factor: 3, floor_docs: 100_000, ..MergePolicy::default() }).unwrap();
    assert_eq!(strata::storage::manifest::read(&dir.path().join("p.strata")).unwrap().segments().count(), 1);
    let queries = rows(10, 6);
    for filter in ["rating >= 4", "rating < 2", "rating >= 2 AND rating < 3"] {
        assert!(recall(&db, &queries, filter) >= 0.9, "{filter}");
    }
    for q in &queries {
        let (got, _) = top(&db, &q.v, "rating >= 4", ", strategy => 'partition'");
        assert!(got.iter().all(|g| rating_of(&data, g).is_some_and(|r| r >= 4.0)), "{got:?}");
        let (got, _) = top(&db, &q.v, "rating < 4", ", strategy => 'partition'");
        assert!(got.iter().all(|g| rating_of(&data, g).is_some_and(|r| r < 4.0)), "{got:?}");
    }
}
