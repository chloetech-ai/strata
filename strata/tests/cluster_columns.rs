//! A coordinator's leaves as Arrow columns (`strata::cluster::merge_columns`,
//! feature `datafusion`): shards in shard mode answer a query that
//! `cluster::concatenates` with Arrow columns, which travel as Arrow IPC
//! and merge into the rows `cluster::merge` makes of the shards' rows: of
//! every column type, with a LIMIT and an OFFSET, a shard whose columns are
//! all NULL, and a shard without rows. A shard that answers rows instead
//! leaves the merge to the rows.
#![cfg(feature = "datafusion")]

use strata::cluster;
use strata::db::Db;
use strata::query::arrow::Columns;
use strata::query::exec::{ExecOptions, ExecStats, Leaf, ResultSet};
use strata::query::{bind, Params};
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema};

const ROWS: usize = 300;

/// A shard holding rows `i` of `0..ROWS` for which `holds(i)`, all but
/// `id`, `n` and `k` NULL in those for which `bare(i)`.
fn shard(dir: &std::path::Path, name: &str, holds: impl Fn(usize) -> bool, bare: impl Fn(usize) -> bool) -> Db {
    let opts = DbOptions { segment_max_docs: 40, sync: false, ..DbOptions::default() };
    let first = Schema::new("l", vec![Field::new("id", FieldType::Int)], Some("id")).unwrap();
    let db = Db::create(&dir.join(name), first, opts).unwrap();
    let none = Params::new();
    db.sql("CREATE TABLE t (id KEYWORD PRIMARY KEY, n INT, x FLOAT, k KEYWORD, ok BOOL, at TIMESTAMP, v VECTOR(3), tags KEYWORD[], nums INT[], j JSON)", &none).unwrap();
    let rows: Vec<String> = (0..ROWS)
        .filter(|&i| holds(i))
        .map(|i| match bare(i) {
            true => format!("('r{i:04}', {}, NULL, 'k{}', NULL, NULL, NULL, NULL, NULL, NULL)", i % 17, i % 7),
            false => format!(
                "('r{i:04}', {}, {}.25, 'k{}', {}, '2026-10-{:02}T08:00:00Z', '[{}.0, 0.5, -1.0]', ARRAY['t{}', 't{}'], ARRAY[{i}, {}], '{{\"a\": {i}, \"b\": \"x{}\"}}')",
                i % 17,
                i % 13,
                i % 7,
                ["TRUE", "FALSE"][i % 2],
                1 + i % 28,
                i % 5,
                i % 3,
                i % 4,
                i * 2,
                i % 3
            ),
        })
        .collect();
    if !rows.is_empty() {
        db.sql(&format!("INSERT INTO t (id, n, x, k, ok, at, v, tags, nums, j) VALUES {}", rows.join(", ")), &none).unwrap();
    }
    db.storage().flush().unwrap();
    db
}

/// Each shard's answer to `sql` in shard mode: as rows, and as Arrow
/// columns sent over Arrow IPC (with the answer's statistics apart), or
/// rows where it has none.
fn answers(shards: &[&Db], sql: &str) -> (strata::query::logical::Select, Vec<ResultSet>, Vec<ResultSet>, Vec<Option<Columns>>) {
    let opts = ExecOptions { shard: true, ..ExecOptions::default() };
    let mut select = None;
    let (mut rows, mut stats, mut columns) = (Vec::new(), Vec::new(), Vec::new());
    for d in shards {
        let view = d.snapshot().table("t").unwrap();
        let q = bind(sql, view.schema(), &Params::new()).unwrap();
        rows.push(d.execute(&view, &q, &opts).unwrap());
        match d.execute_leaf(&view, &q, &opts, 0).unwrap() {
            Leaf::Arrow(c) => {
                let names = c.names.clone();
                let ipc = c.to_ipc().unwrap();
                stats.push(ResultSet { columns: names.clone(), rows: Vec::new(), keys: Vec::new(), scores: Vec::new(), parts: None, stats: c.stats });
                columns.push(Some(Columns::from_ipc(&ipc, names, ExecStats::default()).unwrap()));
            }
            Leaf::Rows(rs) => {
                assert!(rs.rows.is_empty(), "{sql}: a shard with rows answers columns");
                stats.push(rs);
                columns.push(None);
            }
        }
        select = Some(q);
    }
    (select.unwrap(), rows, stats, columns)
}

#[test]
fn shards_columns_merge_as_their_rows() {
    let dir = tempfile::tempdir().unwrap();
    let a = shard(dir.path(), "a.strata", |i| i % 3 == 0, |_| false);
    let b = shard(dir.path(), "b.strata", |i| i % 3 == 1, |_| true);
    let c = shard(dir.path(), "c.strata", |i| i % 3 == 2, |i| i % 2 == 0);
    let empty = shard(dir.path(), "e.strata", |_| false, |_| false);
    let shards = [&a, &b, &empty, &c];
    let queries = [
        "SELECT * FROM t",
        "SELECT id, n, x, k, ok, at, v, tags, nums, j FROM t WHERE n > 3",
        "SELECT k, id, k, nums FROM t WHERE k IN ('k1', 'k2') LIMIT 30",
        "SELECT id, tags, at FROM t LIMIT 25 OFFSET 40",
        "SELECT id FROM t WHERE nums IS NULL OFFSET 10",
        "SELECT n, j FROM t WHERE x > 100",
    ];
    for sql in queries {
        let (q, rows, stats, columns) = answers(&shards, sql);
        assert!(cluster::concatenates(&q), "{sql}");
        let want = cluster::merge(&q, rows).unwrap();
        let some = columns.iter().any(Option::is_some);
        match cluster::merge_columns(&q, &stats, columns).unwrap() {
            Some(got) => {
                let got = got.into_result().unwrap();
                assert_eq!(want.columns, got.columns, "{sql}");
                assert_eq!(want.rows, got.rows, "{sql}");
                assert_eq!(want.keys, got.keys, "{sql}");
                assert_eq!(got.stats.plan.iter().filter(|l| l.starts_with("shard 3:")).count(), want.stats.plan.iter().filter(|l| l.starts_with("shard 3:")).count(), "{sql}");
            }
            // No shard had rows.
            None => assert!(!some && want.rows.is_empty(), "{sql}"),
        }
    }

    // A shard that answers rows (its columns of values of several types)
    // leaves the merge to the rows.
    let sql = "SELECT id, n, tags FROM t";
    let (q, rows, mut stats, mut columns) = answers(&shards, sql);
    stats[1] = rows[1].clone();
    columns[1] = None;
    assert!(cluster::merge_columns(&q, &stats, columns).unwrap().is_none());

    // Rows in an order, or groups, are merged as rows: the shards answer
    // rows.
    for sql in ["SELECT id, n FROM t ORDER BY n DESC LIMIT 5", "SELECT k, COUNT(*) FROM t GROUP BY k", "SELECT COUNT(*) FROM t", "SELECT id, score() FROM t ORDER BY x DESC LIMIT 5"] {
        let view = a.snapshot().table("t").unwrap();
        let q = bind(sql, view.schema(), &Params::new()).unwrap();
        assert!(!cluster::concatenates(&q), "{sql}");
        let opts = ExecOptions { shard: true, ..ExecOptions::default() };
        assert!(matches!(a.execute_leaf(&view, &q, &opts, 0).unwrap(), Leaf::Rows(_)), "{sql}");
    }
}
