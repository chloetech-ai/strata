//! `maxsim(field, vectors)` ranks rows by their best similarity to any of
//! several query vectors, and answers as a brute-force scan does: the
//! union of each query vector's top `k`, scored exactly, holds the exact
//! top `k`. The vectors may come from a subquery of another table, which
//! runs first; `maxsim` fuses like `similarity` and answers alike on one
//! database and over shards. `dot(a, b)` multiplies vectors of rows in
//! memory, and a walk's seeds from another table's subquery carry its
//! rows' scores when PageRank is weighted.

use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::cluster;
use strata::data;
use strata::db::{Db, SqlResult};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::ir::{Expr, Node, Plan};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::vector::kernels::dot_f32;
use strata::{Document, Schema, VectorIndexKind};

const N: usize = 3000;
const DIM: usize = 16;
const SEG: usize = 500;

fn open(dir: &std::path::Path, name: &str, docs: &[Document]) -> Db {
    let opts = DbOptions { segment_max_docs: SEG, sync: false, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db
}

fn vector(v: &Value) -> &[f32] {
    match v {
        Value::Vector(v) => v,
        v => panic!("not a vector: {v:?}"),
    }
}

/// Query vectors near some documents' vectors.
fn queries(docs: &[Document], rng: &mut StdRng, n: usize) -> Vec<Arc<[f32]>> {
    (0..n)
        .map(|_| {
            let d = &docs[rng.gen_range(0..docs.len())];
            vector(&d[data::col::EMB]).iter().map(|x| x + rng.gen_range(-0.05..0.05)).collect::<Vec<f32>>().into()
        })
        .collect()
}

fn list(qs: &[Arc<[f32]>]) -> Value {
    Value::List(qs.iter().map(|q| Value::Vector(q.clone())).collect())
}

/// The top `k` documents passing `keep` by their best dot product with any
/// of `qs`, as the rows' keys and scores.
fn brute(docs: &[Document], qs: &[Arc<[f32]>], keep: impl Fn(&Document) -> bool, k: usize) -> Vec<(String, f64)> {
    let mut all: Vec<(String, f64)> = docs
        .iter()
        .filter(|d| keep(d))
        .map(|d| {
            let v = vector(&d[data::col::EMB]);
            let best = qs.iter().map(|q| dot_f32(v, q)).fold(f32::NEG_INFINITY, f32::max);
            (d[data::col::ID].as_str().unwrap().to_string(), best as f64)
        })
        .collect();
    all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    all.truncate(k);
    all
}

/// A query's answer, compound or not.
fn answer(db: &Db, sql: &str, p: &Params) -> ResultSet {
    match db.sql(sql, p).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Rows(r) => r,
        SqlResult::Written(_) => panic!("{sql}: rows"),
    }
}

fn ranked(rs: &ResultSet) -> Vec<(String, f64)> {
    rs.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect()
}

#[test]
fn maxsim_equals_brute_force() {
    let docs = data::synthetic(N, DIM, 3);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "docs.strata", &docs);
    assert!(db.snapshot().segments().len() > 1, "several segments");
    let mut rng = StdRng::seed_from_u64(5);
    let filters: [(&str, &dyn Fn(&Document) -> bool); 3] = [
        ("", &|_| true),
        ("WHERE year >= 2010", &|d| d[data::col::YEAR].as_i64().unwrap() >= 2010),
        ("WHERE category IN ('c00', 'c01', 'c02')", &|d| ["c00", "c01", "c02"].contains(&d[data::col::CATEGORY].as_str().unwrap())),
    ];
    let mut checked = 0;
    for n in [1, 2, 5, 16, 64] {
        for (filter, keep) in &filters {
            for k in [1, 10, 50] {
                let qs = queries(&docs, &mut rng, n);
                // Every row re-ranked in float32: each query vector's search
                // is exact, and so is the union.
                let sql = format!("SELECT id, score() FROM docs {filter} ORDER BY maxsim(emb, :qs, rerank => {N}) DESC LIMIT {k}");
                let got = ranked(&db.query(&sql, &Params::new().set("qs", list(&qs))).unwrap());
                let want = brute(&docs, &qs, keep, k);
                assert_eq!(got.len(), want.len(), "{sql}");
                for (g, w) in got.iter().zip(&want) {
                    assert_eq!(g.0, w.0, "{sql} with {n} vectors");
                    assert_eq!(g.1, w.1, "{sql}: the score is the best exact dot product");
                }
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 45);

    // One query vector: maxsim is similarity.
    let q = queries(&docs, &mut rng, 1);
    let one = db.query("SELECT id, score() FROM docs ORDER BY maxsim(emb, :qs) DESC LIMIT 20", &Params::new().set("qs", list(&q))).unwrap();
    let sim = db.query("SELECT id, score() FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 20", &Params::new().set("q", Value::Vector(q[0].clone()))).unwrap();
    assert_eq!(ranked(&one), ranked(&sim));

    // With the default re-ranking depth, close to exact.
    let (mut hit, mut total) = (0, 0);
    for _ in 0..20 {
        let qs = queries(&docs, &mut rng, 8);
        let got: Vec<String> = ranked(&db.query("SELECT id, score() FROM docs ORDER BY maxsim(emb, :qs) DESC LIMIT 10", &Params::new().set("qs", list(&qs))).unwrap()).into_iter().map(|r| r.0).collect();
        let want = brute(&docs, &qs, |_| true, 10);
        hit += want.iter().filter(|w| got.contains(&w.0)).count();
        total += want.len();
    }
    assert!(hit as f64 >= 0.95 * total as f64, "recall@10 {hit}/{total}");
}

#[test]
fn the_vectors_are_a_list_a_tuple_json_or_a_subquery() {
    let docs = data::synthetic(N, DIM, 9);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "docs.strata", &docs);
    db.sql(&format!("CREATE TABLE hist (id INT PRIMARY KEY, usr KEYWORD, ts INT, emb VECTOR({DIM}) OPTIONS(index = 'flat'))"), &Params::new()).unwrap();
    let mut rng = StdRng::seed_from_u64(1);
    let qs = queries(&docs, &mut rng, 12);
    let rows: Vec<String> = qs
        .iter()
        .enumerate()
        .map(|(i, q)| format!("({i}, 'u{}', {i}, ARRAY[{}])", i % 2, q.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join(", ")))
        .collect();
    db.sql(&format!("INSERT INTO hist (id, usr, ts, emb) VALUES {}", rows.join(", ")), &Params::new()).unwrap();
    // A user's newest five.
    let mine: Vec<Arc<[f32]>> = (0..12).rev().filter(|i| i % 2 == 1).take(5).map(|i| qs[i].clone()).collect();
    let want = db.query("SELECT id, score() FROM docs ORDER BY maxsim(emb, :qs) DESC LIMIT 15", &Params::new().set("qs", list(&mine))).unwrap();
    assert_eq!(want.rows.len(), 15);
    let sub = "SELECT id, score() FROM docs ORDER BY maxsim(emb, (SELECT emb FROM hist WHERE usr = 'u1' ORDER BY ts DESC LIMIT 5)) DESC LIMIT 15";
    assert_eq!(ranked(&answer(&db, sub, &Params::new())), ranked(&want));
    // A WITH result's column.
    let with = "WITH h AS (SELECT emb, ts FROM hist WHERE usr = 'u1' ORDER BY ts DESC LIMIT 5) SELECT id, score() FROM docs ORDER BY maxsim(emb, (SELECT emb FROM h)) DESC LIMIT 15";
    assert_eq!(ranked(&answer(&db, with, &Params::new())), ranked(&want));
    // Parameters in a tuple, and a JSON array.
    let mut p = Params::new();
    for (i, q) in mine.iter().enumerate() {
        p = p.set(&format!("q{i}"), Value::Vector(q.clone()));
    }
    let tuple = "SELECT id, score() FROM docs ORDER BY maxsim(emb, (:q0, :q1, :q2, :q3, :q4)) DESC LIMIT 15";
    assert_eq!(ranked(&db.query(tuple, &p).unwrap()), ranked(&want));
    let json = serde_json::to_string(&mine.iter().map(|q| q.to_vec()).collect::<Vec<_>>()).unwrap();
    let text = format!("SELECT id, score() FROM docs ORDER BY maxsim(emb, '{json}') DESC LIMIT 15");
    assert_eq!(ranked(&db.query(&text, &Params::new()).unwrap()), ranked(&want));
    // No history: no rows.
    let none = "SELECT id FROM docs ORDER BY maxsim(emb, (SELECT emb FROM hist WHERE usr = 'nobody')) DESC LIMIT 15";
    assert!(answer(&db, none, &Params::new()).rows.is_empty());

    // The plan needs the feature, and EXPLAIN names the vectors.
    let plan = db.explain(sub, &Params::new()).unwrap();
    assert!(plan.features.iter().any(|f| f == "maxsim"), "{:?}", plan.features);
    let text = plan.explain(&db.catalog().iter().map(|s| s.as_ref()).collect::<Vec<&Schema>>()).join("\n");
    assert!(text.contains("maxsim(emb, values of"), "{text}");
    let lines = answer(&db, sub, &Params::new()).stats.plan;
    assert!(lines.iter().any(|l| l.contains("subquery: 5 query vectors")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("maxsim(emb): 5 query vectors")), "{lines:?}");
    assert!(Plan::readable(1, &["maxsim"]).is_ok());
}

#[test]
fn maxsim_refuses_what_it_cannot_score() {
    let docs = data::synthetic(200, DIM, 2);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "docs.strata", &docs);
    let mut rng = StdRng::seed_from_u64(3);
    let e = |sql: &str, p: &Params| db.query(sql, p).unwrap_err().to_string();
    let many = queries(&docs, &mut rng, 65);
    let p = Params::new().set("qs", list(&many));
    assert!(e("SELECT id FROM docs ORDER BY maxsim(emb, :qs) DESC LIMIT 5", &p).contains("at most 64 query vectors"));
    assert_eq!(db.query("SELECT id FROM docs ORDER BY maxsim(emb, :qs, max_vectors => 65) DESC LIMIT 5", &p).unwrap().rows.len(), 5);
    let short = Params::new().set("qs", Value::List(vec![Value::Vector(vec![1.0, 0.0].into())]));
    assert!(e("SELECT id FROM docs ORDER BY maxsim(emb, :qs) DESC LIMIT 5", &short).contains("2 dimensions, field has 16"));
    assert!(e("SELECT id FROM docs ORDER BY maxsim(rating, :qs) DESC LIMIT 5", &p).contains("VECTOR"));
    assert!(e("SELECT id FROM docs ORDER BY maxsim(emb) DESC LIMIT 5", &p).contains("maxsim(field, vectors)"));
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
    let docs = data::synthetic(N, DIM, 7);
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs);
    let a = open(dir.path(), "a.strata", &docs[..N / 2]);
    let b = open(dir.path(), "b.strata", &docs[N / 2..]);
    let mut rng = StdRng::seed_from_u64(11);
    let title = docs[42][data::col::TITLE].as_str().unwrap().to_string();
    let text: Vec<&str> = title.split(' ').take(3).collect();
    let params = Params::new().set("qs", list(&queries(&docs, &mut rng, 6))).set("t", Value::Str(text.join(" ")));
    for sql in [
        "SELECT id, score() FROM docs ORDER BY maxsim(emb, :qs) DESC LIMIT 10",
        "SELECT id, score() FROM docs WHERE rating > 2.5 ORDER BY maxsim(emb, :qs) DESC LIMIT 25",
        "SELECT id, score() FROM docs ORDER BY maxsim(emb, :qs) DESC LIMIT 5 OFFSET 7",
        "SELECT id, score() FROM docs ORDER BY rrf(maxsim(emb, :qs), bm25(body, :t)) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY 0.6 * maxsim(emb, :qs) + 0.4 * bm25(body, :t) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY maxsim(emb, :qs) * rating DESC LIMIT 10",
    ] {
        let want = whole.query(sql, &params).unwrap();
        let got = sharded(&[&a, &b], sql, &params);
        assert_eq!(want.rows.len(), got.rows.len(), "{sql}");
        assert!(!want.rows.is_empty(), "{sql}");
        for (w, g) in want.rows.iter().zip(&got.rows) {
            assert_eq!(w[0], g[0], "{sql}");
            assert!((w[1].as_f64().unwrap() - g[1].as_f64().unwrap()).abs() < 1e-9, "{sql}: {w:?} vs {g:?}");
        }
    }
}

fn rows(db: &Db, sql: &str, p: &Params) -> Vec<Vec<Value>> {
    match db.sql(sql, p).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

#[test]
fn dot_multiplies_vectors_of_rows_in_memory() {
    let docs = data::synthetic(300, DIM, 4);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "docs.strata", &docs);
    db.sql(&format!("CREATE TABLE hist (id INT PRIMARY KEY, usr KEYWORD, emb VECTOR({DIM}) OPTIONS(index = 'flat'))"), &Params::new()).unwrap();
    let mut rng = StdRng::seed_from_u64(8);
    let vs = queries(&docs, &mut rng, 6);
    let values: Vec<String> = vs.iter().enumerate().map(|(i, v)| format!("({i}, 'u', ARRAY[{}])", v.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join(", "))).collect();
    db.sql(&format!("INSERT INTO hist (id, usr, emb) VALUES {}, (6, 'u', NULL)", values.join(", ")), &Params::new()).unwrap();
    let q = queries(&docs, &mut rng, 1).remove(0);
    let p = Params::new().set("q", Value::Vector(q.clone()));
    let got = rows(&db, "WITH h AS (SELECT id, emb FROM hist WHERE usr = 'u') SELECT id, dot(emb, :q) FROM h ORDER BY id", &p);
    assert_eq!(got.len(), 7);
    for (i, r) in got.iter().enumerate() {
        match vs.get(i) {
            // The kernel similarity() uses: equal to the bit.
            Some(v) => assert_eq!(r[1], Value::Float(dot_f32(v, &q) as f64)),
            None => assert_eq!(r[1], Value::Null, "a row without a vector"),
        }
    }
    // On the table itself too: the column is computed from the rows read.
    assert_eq!(rows(&db, "SELECT id, dot(emb, :q) FROM hist WHERE usr = 'u' ORDER BY id", &p), got);
    let best = vs.iter().map(|v| dot_f32(v, &q)).fold(f32::NEG_INFINITY, f32::max) as f64;
    let max = rows(&db, "WITH h AS (SELECT id, emb FROM hist WHERE usr = 'u') SELECT MAX(dot(emb, :q)) FROM h", &p);
    assert_eq!(max, vec![vec![Value::Float(best)]]);
    let plan = db.explain("WITH h AS (SELECT id, emb FROM hist) SELECT id, dot(emb, :q) FROM h", &p).unwrap();
    assert!(plan.features.iter().any(|f| f == "maxsim"), "{:?}", plan.features);
    let e = db.sql("WITH h AS (SELECT id, emb FROM hist) SELECT dot(emb) FROM h", &p).unwrap_err().to_string();
    assert!(e.contains("two vectors"), "{e}");
    let short = Params::new().set("q", Value::Vector(vec![1.0, 2.0].into()));
    let e = db.sql("WITH h AS (SELECT id, emb FROM hist WHERE id = 0) SELECT dot(emb, :q) FROM h", &short).unwrap_err().to_string();
    assert!(e.contains("16 and 2 dimensions"), "{e}");
}

/// Items linked to a few others with a weight each, and users' events
/// naming items with a weight.
fn graph_db(dir: &std::path::Path, rng: &mut StdRng) -> Db {
    let seed = Schema::new("seed", vec![strata::Field::new("id", strata::FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.join("graph.strata"), seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.sql("CREATE TABLE items (id KEYWORD PRIMARY KEY, w FLOAT, cooc EDGES)", &Params::new()).unwrap();
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD, item KEYWORD, w FLOAT)", &Params::new()).unwrap();
    let items: Vec<String> = (0..200)
        .map(|i| {
            let links: Vec<String> = (0..rng.gen_range(1..5)).map(|_| format!("\"i{}\"", rng.gen_range(0..200))).collect();
            format!("('i{i}', {:?}, '[{}]')", rng.gen_range(0.1..1.0f64), links.join(","))
        })
        .collect();
    db.sql(&format!("INSERT INTO items (id, w, cooc) VALUES {}", items.join(", ")), &Params::new()).unwrap();
    db
}

fn ppr(db: &Db, sql: &str) -> Vec<(String, f64)> {
    rows(db, sql, &Params::new()).into_iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect()
}

fn close(a: &[(String, f64)], b: &[(String, f64)], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}");
    let by_key = |v: &[(String, f64)]| v.iter().cloned().collect::<std::collections::HashMap<String, f64>>();
    let (a, b) = (by_key(a), by_key(b));
    for (k, x) in &a {
        let y = b.get(k).unwrap_or_else(|| panic!("{what}: {k} missing"));
        assert!((x - y).abs() <= 1e-9, "{what}: {k} {x} vs {y}");
    }
}

#[test]
fn value_seeds_carry_their_subquerys_scores() {
    let mut rng = StdRng::seed_from_u64(21);
    let dir = tempfile::tempdir().unwrap();
    let db = graph_db(dir.path(), &mut rng);
    let weight: std::collections::HashMap<String, f64> = rows(&db, "SELECT id, w FROM items LIMIT 1000", &Params::new()).into_iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect();
    // Each user's events name 15 items, some twice with the item's weight
    // split between the two.
    let mut events = Vec::new();
    let mut picked: Vec<Vec<String>> = Vec::new();
    for u in 0..4 {
        let mut mine: Vec<String> = Vec::new();
        while mine.len() < 15 {
            let i = format!("i{}", rng.gen_range(0..200));
            if !mine.contains(&i) {
                mine.push(i);
            }
        }
        for item in &mine {
            let w = weight[item];
            if rng.gen_bool(0.4) {
                let part = rng.gen_range(0.2..0.8);
                events.push(format!("({}, 'u{u}', '{item}', {:?})", events.len(), w * part));
                events.push(format!("({}, 'u{u}', '{item}', {:?})", events.len(), w - w * part));
            } else {
                events.push(format!("({}, 'u{u}', '{item}', {w:?})", events.len()));
            }
        }
        picked.push(mine);
    }
    db.sql(&format!("INSERT INTO events (id, usr, item, w) VALUES {}", events.join(", ")), &Params::new()).unwrap();
    for (u, mine) in picked.iter().enumerate() {
        let keys = mine.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", ");
        // The events' weights, summed per item, against the items' own
        // weights on a subquery of the walked table (which ranks seeds by
        // their score).
        let across = ppr(&db, &format!("SELECT id, score() FROM items ORDER BY ppr(cooc, (SELECT item FROM events WHERE usr = 'u{u}' ORDER BY w DESC), weighted => 1, eps => 1e-12) DESC LIMIT 1000"));
        let same = ppr(&db, &format!("SELECT id, score() FROM items ORDER BY ppr(cooc, (SELECT id FROM items WHERE id IN ({keys}) ORDER BY w DESC), weighted => 1, eps => 1e-12) DESC LIMIT 1000"));
        close(&across, &same, "weighted seeds from another table");
        // And against the plan of the keys with the weights written in.
        close(&across, &written(&db, mine, &weight), "weights written in the plan");
        // Unweighted, the scores do not matter: every seed weighs 1.
        let plain = ppr(&db, &format!("SELECT id, score() FROM items ORDER BY ppr(cooc, (SELECT item FROM events WHERE usr = 'u{u}' ORDER BY w DESC), eps => 1e-12) DESC LIMIT 1000"));
        let keys_only = ppr(&db, &format!("SELECT id, score() FROM items ORDER BY ppr(cooc, ({keys}), eps => 1e-12) DESC LIMIT 1000"));
        close(&plain, &keys_only, "unweighted seeds");
        assert_ne!(plain, across, "weights change the walk");
    }
    let lines = answer(&db, "SELECT id FROM items ORDER BY ppr(cooc, (SELECT item FROM events WHERE usr = 'u0' ORDER BY w DESC), weighted => 1) DESC LIMIT 5", &Params::new()).stats.plan;
    assert!(lines.iter().any(|l| l.contains("subquery: 15 weighted seeds")), "{lines:?}");
    // Events ordered by decay (of their weight here, of their time in an
    // application) weigh by it: a seed's weight is the sum of its events'
    // decays.
    let events = "SELECT item, score() FROM events WHERE usr = 'u1' ORDER BY decay(w, 1, 0.5) DESC";
    let mut decays: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for r in rows(&db, events, &Params::new()) {
        *decays.entry(r[0].as_str().unwrap().to_string()).or_default() += r[1].as_f64().unwrap();
    }
    let decayed = ppr(&db, &format!("SELECT id, score() FROM items ORDER BY ppr(cooc, ({}), weighted => 1, eps => 1e-12) DESC LIMIT 1000", events.replace("item, score()", "item")));
    close(&decayed, &written(&db, &picked[1], &decays), "seeds weighted by decay");
}

/// PPR from `keys`, each with its weight written in the plan.
fn written(db: &Db, keys: &[String], weight: &std::collections::HashMap<String, f64>) -> Vec<(String, f64)> {
    let list = keys.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", ");
    let mut plan = db.explain(&format!("SELECT id, score() FROM items ORDER BY ppr(cooc, ({list}), weighted => 1, eps => 1e-12) DESC LIMIT 1000"), &Params::new()).unwrap();
    let mut set = false;
    visit_graphs(&mut plan.root, &mut |g| {
        g.weights = keys.iter().map(|i| weight[i]).collect();
        set = true;
    });
    assert!(set);
    let plan = Plan::new(plan.root);
    assert!(plan.features.iter().any(|f| f == "maxsim"), "{:?}", plan.features);
    let text = plan.explain(&db.catalog().iter().map(|s| s.as_ref()).collect::<Vec<&Schema>>()).join("\n");
    assert!(text.contains("weights => ("), "{text}");
    match db.run(&plan).unwrap() {
        SqlResult::Rows(r) => r.rows.into_iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect(),
        SqlResult::Written(_) => panic!("rows"),
    }
}

fn visit_graphs(n: &mut Node, f: &mut dyn FnMut(&mut strata::query::ir::Graph)) {
    let mut each = |e: &mut Expr| {
        e.visit_mut(&mut |x| {
            if let Expr::Ppr(g) = x {
                f(g);
            }
            Ok(())
        })
        .unwrap()
    };
    match n {
        Node::Project { input, columns } => {
            columns.iter_mut().for_each(|c| each(&mut c.expr));
            visit_graphs(input, f);
        }
        Node::Limit { input, .. } => visit_graphs(input, f),
        Node::Search(s) => s.order.iter_mut().for_each(|k| each(&mut k.expr)),
        _ => {}
    }
}
