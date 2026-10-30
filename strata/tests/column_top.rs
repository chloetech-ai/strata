//! `ORDER BY column LIMIT k` reads the column's blocks best first and
//! stops at the first block that cannot reach the k-th row: it returns the
//! rows a scan of every match returns (`ExecOptions::column_blocks: false`)
//! for INT and FLOAT columns, both directions, NULLS FIRST and LAST, ties
//! (by key and in index order), integers a double cannot tell apart,
//! filters that keep the best values or reject them, probed or
//! materialised, deleted rows and values written in place, over several
//! segments and after merging; and it reads few blocks doing so.

use strata::db::Db;
use strata::query::exec::ExecOptions;
use strata::query::plan::FilterMode;
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, MergePolicy};
use strata::value::{Document, Value};

const N: usize = 30_000;

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![Field::new("id", FieldType::Keyword), Field::new("n", FieldType::Int), Field::new("x", FieldType::Float), Field::new("tag", FieldType::Keyword)],
        Some("id"),
    )
    .unwrap()
}

fn mix(i: usize) -> u64 {
    let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5DEE_CE66;
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^ (x >> 32)
}

/// Row `i`: a heavy-tailed count with many ties and some rows without one
/// (a few beyond 2^53, a double away from each other), a FLOAT with
/// halves, negatives and gaps, and a tag.
fn doc(i: usize) -> Document {
    let h = mix(i);
    let n = match i {
        _ if i % 17 == 0 => Value::Null,
        _ if i % 4999 == 0 => Value::Int((1i64 << 60) + (i % 3) as i64),
        _ => Value::Int(((h % 1000).pow(3) / 1_000_000) as i64),
    };
    let x = match i {
        _ if i % 13 == 0 => Value::Null,
        // One segment's values need the column's float bits.
        _ if i % 7001 == 0 => Value::Float(1e300),
        _ => Value::Float((h >> 20) as f64 % 2000.0 / 2.0 - 300.0),
    };
    let lead = ["a", "b", "q"][(h % 3) as usize];
    vec![Value::Str(format!("{lead}{:05}", i)), n, x, Value::Str(format!("t{}", (h >> 40) % 8))]
}

/// The rows of `sql` as (id, n, x) and the executor's plan lines.
fn run(db: &Db, sql: &str, opts: &ExecOptions) -> (Vec<(String, Value, Value)>, Vec<String>) {
    let r = db.query_with(sql, &Params::new(), opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let rows = r.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].clone(), r[2].clone())).collect();
    (rows, r.stats.plan)
}

/// Every query shape gives the scan's rows, with the filter probed,
/// materialised or as the planner picks; returns how many ran on blocks.
fn check(db: &Db) -> usize {
    let scan = ExecOptions { column_blocks: false, ..ExecOptions::default() };
    let modes = [ExecOptions::default(), ExecOptions { filter_mode: Some(FilterMode::Probe), ..ExecOptions::default() }, ExecOptions { filter_mode: Some(FilterMode::Materialize), ..ExecOptions::default() }];
    let filters = ["", "WHERE PREFIX(id, 'a')", "WHERE tag = 't3'", "WHERE n < 50", "WHERE PREFIX(id, 'q1')", "WHERE x > 100 AND PREFIX(id, 'b')", "WHERE n IS NULL OR tag = 't1'"];
    let orders = ["n DESC", "n", "n DESC NULLS FIRST", "n NULLS FIRST", "x DESC", "x", "x DESC NULLS FIRST"];
    let mut on_blocks = 0;
    for (fi, f) in filters.iter().enumerate() {
        for (oi, o) in orders.iter().enumerate() {
            for tie in [", id", ""] {
                for (k, off) in [(1, 0), (10, 0), (10, 7), (100, 0), (5000, 0)] {
                    let sql = format!("SELECT id, n, x FROM t {f} ORDER BY {o}{tie} LIMIT {k} OFFSET {off}");
                    let (want, _) = run(db, &sql, &scan);
                    let opts = &modes[(fi + oi + k) % modes.len()];
                    let (got, plan) = run(db, &sql, opts);
                    assert_eq!(got, want, "{sql} ({:?})", opts.filter_mode);
                    on_blocks += plan.iter().any(|l| l.contains("blocks read")) as usize;
                }
            }
        }
    }
    on_blocks
}

/// Blocks read and blocks in the column for the top 10 by `n`.
fn blocks_read(db: &Db, sql: &str) -> (usize, usize) {
    let (_, plan) = run(db, sql, &ExecOptions::default());
    let line = plan.iter().find(|l| l.contains("blocks read")).unwrap_or_else(|| panic!("{sql}: {plan:?}"));
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words.iter().position(|w| *w == "of").unwrap();
    (words[at - 1].parse().unwrap(), words[at + 1].parse().unwrap())
}

#[test]
fn top_rows_by_column_from_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("t.strata"), schema(), DbOptions { segment_max_docs: 7000, sync: false, ..DbOptions::default() }).unwrap();
    let mut docs: Vec<Document> = (0..N).map(doc).collect();
    db.load(docs.iter().cloned()).unwrap();
    assert!(db.snapshot().segments().len() >= 4);
    let on = check(&db);
    assert!(on > 50, "{on} on blocks");

    // The best rows from a scan of the documents.
    let r = db.query("SELECT id FROM t ORDER BY n DESC, id LIMIT 10", &Params::new()).unwrap();
    let mut all: Vec<(i64, String)> = docs.iter().filter_map(|d| Some((-d[1].as_i64()?, d[0].as_str()?.to_string()))).collect();
    all.sort();
    let want: Vec<String> = all.into_iter().take(10).map(|(_, id)| id).collect();
    assert_eq!(r.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect::<Vec<_>>(), want);

    // At most a block per row as good as the 10th, with a broad filter or
    // none; a filter that rejects the best values falls back to the scan.
    let reach = |keep: &dyn Fn(&Document) -> bool| {
        let mut ns: Vec<i64> = docs.iter().filter(|d| keep(d)).filter_map(|d| d[1].as_i64()).collect();
        ns.sort_unstable_by(|a, b| b.cmp(a));
        docs.iter().filter(|d| d[1].as_i64().is_some_and(|n| n >= ns[9])).count()
    };
    let (read, total) = blocks_read(&db, "SELECT id, n, x FROM t ORDER BY n DESC LIMIT 10");
    assert!(read <= reach(&|_| true) && read * 4 < total, "{read} of {total}");
    let (read, total) = blocks_read(&db, "SELECT id, n, x FROM t WHERE PREFIX(id, 'a') ORDER BY n DESC, id LIMIT 10");
    assert!(read <= reach(&|d| d[0].as_str().unwrap().starts_with('a')) && read * 4 < total, "{read} of {total}");
    let (_, plan) = run(&db, "SELECT id, n, x FROM t WHERE n < 3 ORDER BY n DESC LIMIT 10", &ExecOptions::default());
    assert!(!plan.iter().any(|l| l.contains("blocks read")), "{plan:?}");

    // Deleted rows and values written in place, the best among them.
    let mut t = db.storage().begin().unwrap();
    for i in (0..N).step_by(61) {
        assert!(t.delete(&docs[i][0]).unwrap());
    }
    for i in (5..N).step_by(97).filter(|i| i % 61 != 0) {
        let v = Value::Int(if i % 2 == 0 { 2_000_000 + i as i64 } else { 0 });
        assert!(t.set(&docs[i][0], "n", v.clone()).unwrap());
        docs[i][1] = v;
        let x = Value::Float(if i % 3 == 0 { 5000.5 } else { -400.0 });
        assert!(t.set(&docs[i][0], "x", x.clone()).unwrap());
        docs[i][2] = x;
    }
    t.commit().unwrap();
    let on = check(&db);
    assert!(on > 50, "{on} on blocks");
    let r = db.query("SELECT id, n FROM t ORDER BY n DESC, id LIMIT 3", &Params::new()).unwrap();
    assert!(r.rows.iter().all(|r| r[1].as_i64().unwrap() >= 1 << 60 || r[1].as_i64().unwrap() > 2_000_000), "{:?}", r.rows);

    db.storage().flush().unwrap();
    let on = check(&db);
    assert!(on > 50, "{on} on blocks");
    while db.storage().merge(&MergePolicy::default()).unwrap().merges > 0 {}
    let on = check(&db);
    assert!(on > 50, "{on} on blocks");
}

/// Clustered by its key, a table keeps each prefix's rows together, so a
/// prefix's top rows read only blocks of its run.
#[test]
fn a_clustered_prefix_reads_its_own_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let schema = schema().with_cluster_by("id").unwrap();
    let db = Db::create(&dir.path().join("c.strata"), schema, DbOptions { segment_max_docs: 7000, sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..N).map(doc)).unwrap();
    let on = check(&db);
    assert!(on > 50, "{on} on blocks");
    // `q1` is a ninth of the rows, a run of each segment: the blocks read
    // are some of its 26.
    let (read, total) = blocks_read(&db, "SELECT id, n, x FROM t WHERE PREFIX(id, 'q1') ORDER BY n DESC, id LIMIT 10");
    assert!(read <= 26 + 8 && total > 200, "{read} of {total}");
    while db.storage().merge(&MergePolicy::default()).unwrap().merges > 0 {}
    let on = check(&db);
    assert!(on > 50, "{on} on blocks");
    let (read, total) = blocks_read(&db, "SELECT id, n, x FROM t WHERE PREFIX(id, 'q1') ORDER BY n DESC, id LIMIT 10");
    assert!(read <= 26 + 8 && total > 200, "{read} of {total}");
}
