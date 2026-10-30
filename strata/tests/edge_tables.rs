//! Edge tables: a table whose rows are the edges of a graph over another
//! table's rows (`OPTIONS(edges = 'src, dst, w')`). A walk over the edge
//! table finds what a walk over an EDGES field holding the same edges
//! finds: the same rows at the same hops for REACHABLE, paths as long for
//! PATH, and the same PageRank, in every direction, through the memtable,
//! flushes and merges, deletes, weights changed in place, and rows that
//! `add_or_insert` creates.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::graph::{edges_value, Edge};
use strata::query::Params;
use strata::storage::{DbOptions, MergePolicy};
use strata::value::Value;
use strata::{Field, FieldType, Schema};

const ITEMS: usize = 300;

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        SqlResult::Rows(r) => r.rows,
        _ => panic!("{sql}: expected rows"),
    }
}

/// Rows with their hops, by key.
fn reached(db: &Db, sql: &str) -> BTreeMap<String, i64> {
    rows(db, sql).into_iter().map(|r| (format!("{:?}", r[0]), r[1].as_i64().unwrap())).collect()
}

fn item(i: usize) -> String {
    format!("i{i}")
}

/// `items` with its edges in an EDGES field, `links`, and `cooc` with the
/// same edges as rows.
fn db(dir: &std::path::Path) -> Db {
    let items = Schema::new("items", vec![Field::new("id", FieldType::Keyword), Field::new("cat", FieldType::Keyword), Field::new("links", FieldType::Edges)], Some("id")).unwrap();
    let opts = DbOptions { segment_max_docs: 70, memtable_chunk_docs: 64, sync: false, ..DbOptions::default() };
    let db = Db::create(&dir.join("e.strata"), items, opts).unwrap();
    run(&db, "CREATE TABLE cooc (id KEYWORD PRIMARY KEY, src KEYWORD NOT NULL REFERENCES items, dst KEYWORD NOT NULL REFERENCES items, w FLOAT DEFAULT 0) OPTIONS(edges = 'src, dst, w')");
    db
}

/// The graph both tables hold: weights by (source, target), and the items
/// that are live.
#[derive(Default)]
struct Model {
    edges: BTreeMap<(usize, usize), f32>,
    live: BTreeSet<usize>,
}

impl Model {
    fn links(&self, i: usize) -> Value {
        let out: Vec<Edge> = self.edges.range((i, 0)..(i + 1, 0)).map(|(&(_, j), &w)| Edge::to(item(j)).weight(w)).collect();
        edges_value(&out)
    }
    fn item_row(&self, i: usize) -> Vec<Value> {
        vec![Value::Str(item(i)), Value::Str(format!("c{}", i % 5)), self.links(i)]
    }
}

fn cooc_key(a: usize, b: usize) -> String {
    format!("{}>{}", item(a), item(b))
}

fn cooc_row(a: usize, b: usize, w: f32) -> Vec<Value> {
    vec![Value::Str(cooc_key(a, b)), Value::Str(item(a)), Value::Str(item(b)), Value::Float(w as f64)]
}

/// A weight with an exact f32 value, so that both tables hold the same.
fn weight(rng: &mut rand::rngs::StdRng) -> f32 {
    (rng.gen_range(1..64) as f32) / 16.0
}

/// Rows by key with their scores, as a map.
fn scored(db: &Db, sql: &str) -> HashMap<String, f64> {
    rows(db, sql).into_iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect()
}

/// Compares walks over both, from `seeds`; the rows they reached.
fn check(db: &Db, seeds: &[String], what: &str) -> usize {
    let mut seen = 0;
    for dir in ["out", "in", "both"] {
        for s in seeds {
            for hops in [1, 3] {
                let q = |g: &str| format!("SELECT id, hops() FROM items WHERE REACHABLE({g}, '{s}', hops => {hops}, dir => '{dir}') LIMIT 1000");
                let (field, table) = (reached(db, &q("links")), reached(db, &q("cooc")));
                assert_eq!(field, table, "{what}: REACHABLE from {s}, {hops} hops, {dir}");
            }
            let q = |g: &str| format!("SELECT id, score() FROM items ORDER BY ppr({g}, '{s}', eps => 1e-10, dir => '{dir}') DESC LIMIT 1000");
            let (field, table) = (scored(db, &q("links")), scored(db, &q("cooc")));
            seen += field.len();
            let keys = |m: &HashMap<String, f64>| m.iter().filter(|(_, &v)| v > 1e-7).map(|(k, _)| k.clone()).collect::<BTreeSet<_>>();
            assert_eq!(keys(&field), keys(&table), "{what}: ppr from {s}, {dir}");
            for (k, v) in &field {
                let t = table.get(k).copied().unwrap_or(0.0);
                assert!((v - t).abs() < 1e-6, "{what}: ppr from {s}, {dir}, row {k}: {v} over the field, {t} over the edge table");
            }
        }
        // Several seeds, and a seed list weighted by a subquery's scores.
        let list = format!("({})", seeds.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", "));
        let q = |g: &str| format!("SELECT id, score() FROM items ORDER BY ppr({g}, {list}, eps => 1e-10, dir => '{dir}') DESC LIMIT 1000");
        let (field, table) = (scored(db, &q("links")), scored(db, &q("cooc")));
        for (k, v) in &field {
            assert!((v - table.get(k).copied().unwrap_or(0.0)).abs() < 1e-6, "{what}: ppr from {list}, {dir}, row {k}");
        }
    }
    for w in seeds.windows(2) {
        let q = |g: &str| format!("SELECT id FROM items WHERE PATH({g}, '{}', '{}', max_hops => 8) LIMIT 100", w[0], w[1]);
        assert_eq!(rows(db, &q("links")).len(), rows(db, &q("cooc")).len(), "{what}: PATH from {} to {}", w[0], w[1]);
    }
    // A walk that passes only through some rows.
    let s = &seeds[0];
    let q = |g: &str| format!("SELECT id, hops() FROM items WHERE REACHABLE({g}, '{s}', hops => 4, through => cat != 'c0') LIMIT 1000");
    assert_eq!(reached(db, &q("links")), reached(db, &q("cooc")), "{what}: REACHABLE through");
    seen
}

#[test]
fn an_edge_table_walks_as_the_same_edges_field() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(5);
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let mut m = Model::default();
    for i in 0..ITEMS {
        m.live.insert(i);
        for _ in 0..rng.gen_range(0..5) {
            m.edges.insert((i, rng.gen_range(0..ITEMS)), weight(&mut rng));
        }
    }
    // Rows in batches, flushed into several segments, the last in the
    // memtable.
    let all: Vec<usize> = (0..ITEMS).collect();
    for chunk in all.chunks(80) {
        let mut txn = db.storage().begin().unwrap();
        for &i in chunk {
            txn.table("items").unwrap().insert(m.item_row(i)).unwrap();
            for (&(a, b), &w) in m.edges.range((i, 0)..(i + 1, 0)) {
                txn.table("cooc").unwrap().insert(cooc_row(a, b, w)).unwrap();
            }
        }
        txn.commit().unwrap();
        if chunk[0] < 200 {
            db.storage().flush().unwrap();
        }
    }
    assert!(db.snapshot().table("cooc").unwrap().segments().len() >= 5);
    let seeds: Vec<String> = (0..6).map(|_| item(rng.gen_range(0..ITEMS))).collect();
    assert!(check(&db, &seeds, "loaded") > 1000);

    // Edges deleted, weights changed in place, new edges created by
    // add_or_insert, a weight set to zero, and items deleted.
    let mut txn = db.storage().begin().unwrap();
    let keys: Vec<(usize, usize)> = m.edges.keys().copied().collect();
    let mut touched = BTreeSet::new();
    for _ in 0..120 {
        let (a, b) = keys[rng.gen_range(0..keys.len())];
        let Some(&w) = m.edges.get(&(a, b)) else { continue };
        touched.insert(a);
        match rng.gen_range(0..4) {
            0 => {
                m.edges.remove(&(a, b));
                assert!(txn.table("cooc").unwrap().delete(&Value::Str(cooc_key(a, b))).unwrap());
            }
            1 => {
                let d = weight(&mut rng);
                m.edges.insert((a, b), w + d);
                assert!(txn.table("cooc").unwrap().add(&Value::Str(cooc_key(a, b)), "w", &Value::Float(d as f64)).unwrap());
            }
            2 => {
                m.edges.remove(&(a, b));
                assert!(txn.table("cooc").unwrap().set(&Value::Str(cooc_key(a, b)), "w", Value::Float(0.0)).unwrap());
            }
            _ => {
                let c = rng.gen_range(0..ITEMS);
                let d = weight(&mut rng);
                let had = m.edges.get(&(a, c)).copied();
                m.edges.insert((a, c), had.unwrap_or(0.0) + d);
                let mut row = cooc_row(a, c, 0.0);
                row[3] = Value::Null;
                txn.table("cooc").unwrap().add_or_insert(row, "w", &Value::Float(d as f64)).unwrap();
            }
        }
    }
    for &a in &touched {
        txn.table("items").unwrap().upsert(m.item_row(a)).unwrap();
    }
    for _ in 0..10 {
        let i = rng.gen_range(0..ITEMS);
        if m.live.remove(&i) {
            txn.table("items").unwrap().delete(&Value::Str(item(i))).unwrap();
        }
    }
    txn.commit().unwrap();
    let seeds: Vec<String> = seeds.into_iter().filter(|s| m.live.contains(&s[1..].parse().unwrap())).collect();
    assert!(seeds.len() >= 3);
    assert!(check(&db, &seeds, "changed") > 1000);

    db.storage().flush().unwrap();
    assert!(check(&db, &seeds, "flushed") > 1000);
    let before = db.snapshot().table("cooc").unwrap().segments().len();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
    assert!(db.snapshot().table("cooc").unwrap().segments().len() < before);
    assert!(check(&db, &seeds, "merged") > 1000);

    // The weights as the model has them, and each live edge's row.
    let n: i64 = rows(&db, "SELECT COUNT(*) FROM cooc WHERE w > 0")[0][0].as_i64().unwrap();
    assert_eq!(n as usize, m.edges.len());

    // Reopened, the walk reads the same sections.
    let path = dir.path().join("e.strata");
    drop(db);
    let db = Db::open(&path, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    assert!(check(&db, &seeds, "reopened") > 1000);
}

#[test]
fn integer_keys_and_unweighted_edges() {
    let dir = tempfile::tempdir().unwrap();
    let people = Schema::new("people", vec![Field::new("id", FieldType::Int), Field::new("links", FieldType::Edges)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("p.strata"), people, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    run(&db, "CREATE TABLE knows (id INT PRIMARY KEY, a INT NOT NULL, b INT NOT NULL) OPTIONS(edges = 'a, b')");
    let edges = [(1, 2), (2, 3), (3, 4), (1, 5), (5, 4), (6, 1)];
    let mut txn = db.storage().begin().unwrap();
    for p in 1..=6i64 {
        let out: Vec<Edge> = edges.iter().filter(|e| e.0 == p).map(|e| Edge::to(Value::Int(e.1))).collect();
        txn.table("people").unwrap().insert(vec![Value::Int(p), edges_value(&out)]).unwrap();
    }
    for (i, (a, b)) in edges.iter().enumerate() {
        txn.table("knows").unwrap().insert(vec![Value::Int(i as i64), Value::Int(*a), Value::Int(*b)]).unwrap();
    }
    txn.commit().unwrap();
    let ids = |g: &str, hops: u32, dir: &str| reached(&db, &format!("SELECT id, hops() FROM people WHERE REACHABLE({g}, 1, hops => {hops}, dir => '{dir}')"));
    for (hops, dir) in [(1, "out"), (2, "out"), (3, "both"), (2, "in")] {
        assert_eq!(ids("knows", hops, dir), ids("links", hops, dir), "{hops} hops {dir}");
    }
    assert_eq!(ids("knows", 2, "out").len(), 4);
    let path = rows(&db, "SELECT id FROM people WHERE PATH(knows, 6, 4) ORDER BY hops()");
    assert_eq!(path.len(), 4);
    // The ends of an edge are not changed in place.
    let mut txn = db.storage().begin().unwrap();
    let e = txn.table("knows").unwrap().set(&Value::Int(0), "b", Value::Int(4)).unwrap_err();
    assert!(e.to_string().contains("links the rows (edges)"), "{e}");
    let e = txn.table("knows").unwrap().add(&Value::Int(0), "a", &Value::Int(1)).unwrap_err();
    assert!(e.to_string().contains("not changed in place"), "{e}");
    drop(txn);
    let e = db.sql("UPDATE knows SET a = 5 WHERE id = 0", &Params::new()).unwrap_err();
    assert!(e.to_string().contains("links the rows (edges)"), "{e}");
}

#[test]
fn add_or_insert_starts_a_row_or_adds_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let w = |db: &Db, k: &str| rows(db, &format!("SELECT w FROM cooc WHERE id = '{k}'")).first().map(|r| r[0].as_f64().unwrap());
    for (delta, want) in [(1.5, 1.5), (2.0, 3.5)] {
        let mut txn = db.storage().begin().unwrap();
        let mut row = cooc_row(1, 2, 0.0);
        row[3] = Value::Null;
        txn.table("cooc").unwrap().add_or_insert(row, "w", &Value::Float(delta)).unwrap();
        txn.commit().unwrap();
        assert_eq!(w(&db, &cooc_key(1, 2)), Some(want));
    }
    // A row's own value is the base when it is created; twice in one
    // transaction, the second adds to the first.
    let mut txn = db.storage().begin().unwrap();
    txn.table("cooc").unwrap().add_or_insert(cooc_row(3, 4, 10.0), "w", &Value::Int(1)).unwrap();
    txn.table("cooc").unwrap().add_or_insert(cooc_row(3, 4, 10.0), "w", &Value::Int(1)).unwrap();
    txn.commit().unwrap();
    assert_eq!(w(&db, &cooc_key(3, 4)), Some(12.0));
    let mut txn = db.storage().begin().unwrap();
    let e = txn.table("cooc").unwrap().add_or_insert(cooc_row(5, 6, 0.0), "id", &Value::Int(1)).unwrap_err();
    assert!(e.to_string().contains("not a numeric field"), "{e}");
}

#[test]
fn walks_and_options_that_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    run(&db, "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD, item KEYWORD REFERENCES items)");
    run(&db, "CREATE TABLE ints (id INT PRIMARY KEY, a INT, b INT) OPTIONS(edges = 'a, b')");
    run(&db, "CREATE TABLE other (id KEYWORD PRIMARY KEY)");
    run(&db, "CREATE TABLE elsewhere (id KEYWORD PRIMARY KEY, a KEYWORD REFERENCES other, b KEYWORD REFERENCES other) OPTIONS(edges = 'a, b')");
    let fails = |sql: &str, want: &str| {
        let e = db.query(sql, &Params::new()).unwrap_err().to_string();
        assert!(e.contains(want), "{sql}: {e}");
    };
    fails("SELECT id FROM items ORDER BY ppr(events, 'i1') DESC LIMIT 5", "events is not an edge table");
    fails("SELECT id FROM items ORDER BY ppr(ints, 'i1') DESC LIMIT 5", "ints links INT keys, and items's are KEYWORD");
    fails("SELECT id FROM items ORDER BY ppr(elsewhere, 'i1') DESC LIMIT 5", "elsewhere links rows of other, not of items");
    fails("SELECT id FROM items WHERE REACHABLE(cooc, 'i1', label => 'x')", "have no labels");
    fails("SELECT id FROM items ORDER BY ppr(nothing, 'i1') DESC LIMIT 5", "nothing");
    for (options, want) in [
        ("edges = 'src'", "name the source and target"),
        ("edges = 'src, src'", "two stored, single-valued KEYWORD or INT fields of one type"),
        ("edges = 'src, n'", "of one type"),
        ("edges = 'src, dst, tag'", "the weight tag is a stored, single-valued FLOAT or INT field"),
        ("edges = 'src, dst, gone'", "the weight gone is not a field"),
        ("edges = 3", "in quotes"),
    ] {
        let sql = format!("CREATE TABLE bad (id KEYWORD PRIMARY KEY, src KEYWORD, dst KEYWORD, n INT, tag KEYWORD) OPTIONS({options})");
        let e = db.sql(&sql, &Params::new()).unwrap_err().to_string();
        assert!(e.contains(want), "{options}: {e}");
    }
}

#[test]
fn the_option_round_trips_and_names_its_features() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let cooc = db.snapshot().table("cooc").unwrap().schema().clone();
    let short = cooc.to_short_json();
    assert_eq!(short["edges"], "src, dst, w");
    assert_eq!(Schema::from_json(&short.to_string()).unwrap(), cooc);
    let long = serde_json::to_string(&cooc).unwrap();
    assert_eq!(Schema::from_json(&long).unwrap(), cooc);
    assert!(cooc.features().any(|f| f == "edge_table"));

    let mut txn = db.storage().begin().unwrap();
    txn.table("items").unwrap().insert(vec![Value::Str(item(1)), Value::Str("c1".into()), Value::Null]).unwrap();
    txn.table("items").unwrap().insert(vec![Value::Str(item(2)), Value::Str("c2".into()), Value::Null]).unwrap();
    txn.table("cooc").unwrap().insert(cooc_row(1, 2, 1.0)).unwrap();
    txn.commit().unwrap();
    let sql = "SELECT id, score() FROM items ORDER BY ppr(cooc, 'i1') DESC LIMIT 5";
    let plan = db.explain(sql, &Params::new()).unwrap();
    assert!(plan.features.iter().any(|f| f == "edge_table"), "{:?}", plan.features);
    let text = plan.explain(&[]).join("\n");
    assert!(text.contains("ppr(cooc, 'i1'"), "{text}");
    // The plan read back from JSON walks the same edges; a reader without
    // the feature refuses it by name.
    let again = strata::query::ir::Plan::from_json(&plan.to_json().unwrap()).unwrap();
    let rs = db.run_at(&db.snapshot(), &again, &Default::default()).unwrap();
    assert_eq!(rs.rows.len(), 2);
    let e = strata::query::ir::Plan::readable_by(1, &["edge_table"], &[]).unwrap_err().to_string();
    assert!(e.contains("edge_table"), "{e}");
}
