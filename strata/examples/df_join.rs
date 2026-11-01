//! Times compound queries (joins, groups over joined rows, `DISTINCT`,
//! `WITH`, window functions) on Strata's operators (`strata::query::ops`) and on DataFusion
//! (`strata::query::df`), over two tables of `n` rows each, and checks
//! that both give the same rows.
//!
//! ```text
//! cargo run --release --features datafusion --example df_join -- 1000000
//! ```
//!
//! Prints, per query, the median of three runs (more for fast queries,
//! one past ten seconds): the operators (one thread, as they run), DataFusion on one
//! partition and on four, the default engine (`auto`: the operators when
//! the leaves read few rows, DataFusion otherwise) and which it chose,
//! and the leaves alone (the single-table scans every engine runs first).
//! `DF_ONLY=i` runs query `i` alone.

use std::time::Instant;

use strata::db::Db;
use strata::query::exec::{Engine, ExecOptions, ResultSet};
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema};

const QUERIES: &[(&str, &str, &[&str])] = &[
    (
        "join + GROUP BY",
        "SELECT a.g, COUNT(*), SUM(b.z) FROM a JOIN b ON b.a_id = a.id GROUP BY a.g",
        &["SELECT id, g FROM a", "SELECT a_id, z FROM b"],
    ),
    (
        "join + filter + ORDER BY LIMIT",
        "SELECT a.id, b.z FROM a JOIN b ON b.a_id = a.id WHERE b.z > 0.5 ORDER BY b.z DESC, a.id LIMIT 10",
        &["SELECT id FROM a", "SELECT a_id, z FROM b WHERE z > 0.5"],
    ),
    (
        "join + DISTINCT",
        "SELECT DISTINCT a.g, b.tag FROM a JOIN b ON b.a_id = a.id",
        &["SELECT id, g FROM a", "SELECT a_id, tag FROM b"],
    ),
    (
        "WITH + join + GROUP BY",
        "WITH t AS (SELECT a_id, z FROM b WHERE z > 0.9) SELECT a.g, COUNT(*) FROM a JOIN t ON t.a_id = a.id GROUP BY a.g",
        &["SELECT a_id, z FROM b WHERE z > 0.9", "SELECT id, g FROM a"],
    ),
    (
        "selective join (1,000 left rows)",
        "SELECT a.id, b.z FROM a JOIN b ON b.a_id = a.id WHERE a.x < 1000",
        &["SELECT id FROM a WHERE x < 1000"],
    ),
    (
        "window: the top 3 of each group (QUALIFY)",
        "SELECT id, g, x FROM a QUALIFY ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC) <= 3",
        &["SELECT id, g, x FROM a"],
    ),
    (
        "window: a moving sum and a lag",
        "SELECT id, SUM(y) OVER (ORDER BY x ROWS BETWEEN 9 PRECEDING AND CURRENT ROW), LAG(x) OVER (ORDER BY id) FROM a ORDER BY id LIMIT 10",
        &["SELECT id, x, y FROM a"],
    ),
    (
        "window: groups ranked",
        "SELECT g, COUNT(*), RANK() OVER (ORDER BY SUM(y) DESC) FROM a GROUP BY g",
        &["SELECT g, y FROM a"],
    ),
];

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

/// The median of three runs (up to 51 while they take under 300 ms in
/// all), or the first alone when it took over ten seconds.
fn time(f: &mut dyn FnMut() -> ResultSet) -> (f64, ResultSet) {
    let mut ms = Vec::new();
    let mut last = None;
    // Three runs, more (up to 51) while they take under 300 ms in all.
    while ms.len() < 3 || (ms.len() < 51 && ms.iter().sum::<f64>() < 300.0) {
        let t = Instant::now();
        let rs = f();
        ms.push(t.elapsed().as_secs_f64() * 1e3);
        last = Some(rs);
        if ms[0] > 10_000.0 {
            break;
        }
    }
    (median(ms), last.expect("a run"))
}

/// A row's sort key, with floats rounded: the engines add floats in other
/// orders, so a `SUM` of floats can differ in its last digits.
fn key(row: &[Value]) -> String {
    row.iter().map(|v| match v {
        Value::Float(f) => format!("{f:.6e}"),
        v => format!("{v:?}"),
    }).collect::<Vec<_>>().join("|")
}

/// Panics unless both engines gave the same rows: in the same order under
/// `ORDER BY`, as the same multiset otherwise, floats within 1e-9 relative.
fn agree(sql: &str, want: &ResultSet, got: &ResultSet) {
    assert_eq!(want.rows.len(), got.rows.len(), "{sql}: rows");
    let (mut a, mut b): (Vec<&Vec<Value>>, Vec<&Vec<Value>>) = (want.rows.iter().collect(), got.rows.iter().collect());
    if !sql.contains("ORDER BY") {
        a.sort_by_key(|r| key(r));
        b.sort_by_key(|r| key(r));
    }
    for (x, y) in a.iter().zip(&b) {
        let same = x.len() == y.len()
            && x.iter().zip(y.iter()).all(|(p, q)| match (p, q) {
                (Value::Float(p), Value::Float(q)) => (p - q).abs() <= 1e-9 * p.abs().max(q.abs()).max(1.0),
                (p, q) => p == q,
            });
        assert!(same, "{sql}: the engines disagree: {x:?} against {y:?}");
    }
}

fn main() {
    let n: i64 = std::env::args().nth(1).map_or(1_000_000, |a| a.replace('_', "").parse().expect("rows"));
    let dir = tempfile::tempdir().expect("a directory");
    let t = Instant::now();
    let a = Schema::new("a", vec![Field::new("id", FieldType::Int), Field::new("g", FieldType::Keyword), Field::new("x", FieldType::Int), Field::new("y", FieldType::Float)], Some("id")).expect("schema");
    let db = Db::create(&dir.path().join("bench.strata"), a, DbOptions { sync: false, ..DbOptions::default() }).expect("db");
    // A cheap permutation, so that `x` is no copy of `id`.
    let mix = |i: i64| (i.wrapping_mul(2_654_435_761) % n + n) % n;
    db.load((0..n).map(|i| vec![Value::Int(i), Value::Str(format!("g{}", mix(i) % 1000)), Value::Int(mix(i)), Value::Float((mix(i) % 997) as f64 / 997.0)])).expect("load a");
    db.sql("CREATE TABLE b (id INT PRIMARY KEY, a_id INT, z FLOAT, tag KEYWORD)", &Params::new()).expect("table b");
    db.load_into("b", (0..n).map(|i| vec![Value::Int(i), Value::Int(mix(i)), Value::Float((i % 1009) as f64 / 1009.0), Value::Str(format!("t{}", i % 50))])).expect("load b");
    eprintln!("loaded 2 x {n} rows in {:.1} s", t.elapsed().as_secs_f64());
    let cpu = std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|c| c.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())).unwrap_or_default();
    println!("{n} rows a table; {} cores; {cpu}", std::thread::available_parallelism().map_or(0, |n| n.get()));
    println!("| query | rows | operators | DataFusion, 1 partition | DataFusion, 4 partitions | auto, 4 partitions | leaves alone |");
    println!("|---|---|---|---|---|---|---|");
    let ms = |t: f64| if t < 10.0 { format!("{t:.1} ms") } else { format!("{t:.0} ms") };
    let only: Option<usize> = std::env::var("DF_ONLY").ok().and_then(|v| v.parse().ok());
    for (qi, (name, sql, leaves)) in QUERIES.iter().enumerate() {
        if only.is_some_and(|o| o != qi) {
            continue;
        }
        let run = |engine: Engine, threads: usize| ExecOptions { engine, threads, ..db.exec.clone() };
        let ops = run(Engine::Operators, 1);
        let (t_ops, want) = match db.query_with(sql, &Params::new(), &ops) {
            Ok(_) => {
                let (t, rs) = time(&mut || db.query_with(sql, &Params::new(), &ops).expect("operators"));
                (ms(t), Some(rs))
            }
            Err(e) => (format!("fails: {e}"), None),
        };
        let mut cells = Vec::new();
        let mut rows = 0;
        for threads in [1, 4] {
            let o = run(Engine::DataFusion, threads);
            let (t, got) = time(&mut || db.query_with(sql, &Params::new(), &o).expect("datafusion"));
            if got.stats.plan.iter().any(|l| l.starts_with("operators (not datafusion")) {
                panic!("{sql}: not on DataFusion: {:?}", got.stats.plan.first());
            }
            if let Some(w) = &want {
                agree(sql, w, &got);
            }
            rows = got.rows.len();
            cells.push(ms(t));
        }
        let o = run(Engine::Auto, 4);
        let (t, got) = time(&mut || db.query_with(sql, &Params::new(), &o).expect("auto"));
        if let Some(w) = &want {
            agree(sql, w, &got);
        }
        let engine = if got.stats.plan.iter().any(|l| l.starts_with("datafusion:")) { "DataFusion" } else { "operators" };
        cells.push(format!("{} ({engine})", ms(t)));
        let leaf_ms: f64 = leaves.iter().map(|l| time(&mut || db.query_with(l, &Params::new(), &run(Engine::Operators, 1)).expect("leaf")).0).sum();
        println!("| {name} | {rows} | {t_ops} | {} | {} | {} | {} |", cells[0], cells[1], cells[2], ms(leaf_ms));
    }
}
