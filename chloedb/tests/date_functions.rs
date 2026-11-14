//! Date functions: `date_trunc`, `date_bin`, `date_part` and `EXTRACT`,
//! and `strftime`, over a TIMESTAMP column. As values of rows they are the row
//! evaluator's; a `GROUP BY` of one over a table's TIMESTAMP field is the
//! executor's, read from the field's column (a part that is a number from
//! the columns, with the query's other groups and aggregates), and its
//! answer is the one the operators give computing the function row by
//! row: the same groups with the same aggregates. Checked on rows in the
//! memtable, across flushed segments, after deletes and updates, and after
//! a merge.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use chloedb::db::{Db, SqlResult};
use chloedb::query::compound::plan_read_by_rows;
use chloedb::query::Params;
use chloedb::schema::Schema;
use chloedb::storage::{DbOptions, Layout, MergePolicy};
use chloedb::timestamp;
use chloedb::value::Value;

fn opts() -> DbOptions {
    DbOptions { sync: false, layout: Layout::SingleFile, segment_max_docs: 150, ..DbOptions::default() }
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn open(dir: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![chloedb::schema::Field::new("id", chloedb::schema::FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.join("d.strata"), seed, opts()).unwrap();
    sql(&db, "CREATE TABLE snaps (id INT PRIMARY KEY, at TIMESTAMP, total FLOAT, kind KEYWORD)");
    db
}

/// 2026-08-01, and times over the 90 days from it, some before 1970 and
/// some without a time.
const START: i64 = 1_785_542_400_000_000;

fn at(rng: &mut StdRng) -> Option<i64> {
    match rng.gen_range(0..40) {
        0 => None,
        1 => Some(-rng.gen_range(0..90 * 86_400_000_000i64)),
        _ => Some(START + rng.gen_range(0..90 * 86_400_000_000i64)),
    }
}

/// Inserts rows `from..to`, flushing every `flush` rows (0: never).
fn fill(db: &Db, rng: &mut StdRng, from: usize, to: usize, flush: usize) {
    for i in from..to {
        let t = at(rng).map_or("NULL".to_string(), |t| format!("'{}'", timestamp::format(t)));
        let kind = ["auction", "fixed", "best_offer"][rng.gen_range(0..3)];
        sql(db, &format!("INSERT INTO snaps VALUES ({i}, {t}, {:.2}, '{kind}')", rng.gen_range(5.0..500.0)));
        if flush > 0 && (i + 1) % flush == 0 {
            db.storage().flush().unwrap();
        }
    }
}

const KEYS: [&str; 13] = [
    "date_trunc('day', at)",
    "date_bin('6 hours', at)",
    "date_bin('90 minutes', at, '2026-08-01T00:17:00Z')",
    "date_bin('1 day', at)",
    "date_bin('7d', at, '2026-08-03')",
    "date_trunc('week', at)",
    "date_trunc('MONTH', at)",
    "EXTRACT(dow FROM at)",
    "date_part('hour', at)",
    "EXTRACT(SECOND FROM at)",
    "strftime('%Y-%m', at)",
    "time_bucket('6 hours', at)",
    "time_bucket(INTERVAL '90 minutes', at)",
];

fn queries() -> Vec<String> {
    let mut out = Vec::new();
    for k in KEYS {
        out.push(format!("SELECT {k} AS g, COUNT(*) FROM snaps GROUP BY g"));
        out.push(format!("SELECT {k}, COUNT(*), AVG(total), MIN(total), MAX(at) FROM snaps WHERE kind = 'fixed' GROUP BY {k}"));
        out.push(format!("SELECT kind, {k} AS g, SUM(total) FROM snaps GROUP BY kind, g HAVING COUNT(*) > 2"));
        out.push(format!("SELECT {k} AS g, COUNT(*) AS n FROM snaps WHERE total > 100 GROUP BY 1 ORDER BY g DESC LIMIT 7"));
        out.push(format!("SELECT {k} AS g, COUNT(*) AS n FROM snaps GROUP BY g ORDER BY n DESC, g LIMIT 5"));
    }
    out
}

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut out: Vec<String> = rows.drain(..).map(|r| format!("{r:?}")).collect();
    out.sort();
    out
}

/// Every query's answer, against the plan that computes the function row
/// by row.
fn check(db: &Db, stage: &str) {
    let catalog = db.catalog();
    let tables: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
    for q in queries() {
        let got = db.query(&q, &Params::new()).unwrap_or_else(|e| panic!("{stage}: {q}: {e}"));
        let plan = db.explain(&q, &Params::new()).unwrap();
        assert!(plan.features.iter().any(|f| f == "dates"), "{stage}: {q}: {:?}", plan.features);
        assert_eq!(plan.features.iter().any(|f| f == "date_bin"), q.contains("date_bin") || q.contains("time_bucket"), "{stage}: {q}: {:?}", plan.features);
        assert!(got.stats.plan.iter().any(|l| l.contains("aggregate") && l.contains("rows into groups")), "{stage}: {q}: {:?}", got.stats.plan);
        // A part that is a number is grouped by from the columns; text
        // (strftime) row by row.
        let columns = got.stats.plan.iter().any(|l| l.contains("rows into groups") && l.contains("from the columns"));
        assert_eq!(columns, !q.contains("strftime"), "{stage}: {q}: {:?}", got.stats.plan);
        let by_rows = plan_read_by_rows(&q, &tables, &Params::new()).unwrap();
        let SqlResult::Rows(want) = db.run(&by_rows).unwrap_or_else(|e| panic!("{stage}: {q}, by rows: {e}")) else { panic!("rows") };
        assert!(!want.stats.plan.iter().any(|l| l.contains("rows into groups")), "{stage}: {q}: by rows: {:?}", want.stats.plan);
        assert_eq!(got.columns, want.columns, "{stage}: {q}");
        assert!(!got.rows.is_empty() || q.contains("HAVING"), "{stage}: {q}");
        if q.contains("LIMIT") {
            assert_eq!(got.rows, want.rows, "{stage}: {q}");
        } else {
            assert_eq!(sorted(got.rows), sorted(want.rows), "{stage}: {q}");
        }
    }
}

#[test]
fn groups_by_parts_of_times_answer_as_rows_computed_one_by_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let mut rng = StdRng::seed_from_u64(11);
    fill(&db, &mut rng, 0, 120, 0);
    check(&db, "memtable");
    fill(&db, &mut rng, 120, 900, 150);
    check(&db, "segments");
    sql(&db, "DELETE FROM snaps WHERE id BETWEEN 300 AND 380 OR id IN (5, 77, 140)");
    sql(&db, "UPDATE snaps SET at = '2026-12-25T10:30:00Z' WHERE id BETWEEN 500 AND 540");
    sql(&db, "UPDATE snaps SET total = 1.5 WHERE kind = 'fixed' AND id < 200");
    check(&db, "deletes and updates");
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    check(&db, "merged");
}

#[test]
fn values_of_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    sql(&db, "INSERT INTO snaps VALUES (1, '2026-11-05T07:31:52.25Z', 10, 'fixed'), (2, NULL, 20, 'fixed'), (3, '1969-12-31T23:00:00Z', 30, 'auction')");
    let rows = sql(&db, "SELECT id, date_trunc('week', at), EXTRACT(isodow FROM at), date_part('doy', at), strftime('%Y-%m-%d %H:%M', at), EXTRACT(epoch FROM at) FROM snaps ORDER BY id");
    let ts = |s: &str| Value::Timestamp(timestamp::parse(s).unwrap());
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), ts("2026-11-02"), Value::Int(4), Value::Int(309), Value::Str("2026-11-05 07:31".into()), Value::Float(1_793_863_912.25)],
            vec![Value::Int(2), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null],
            vec![Value::Int(3), ts("1969-12-29"), Value::Int(3), Value::Int(365), Value::Str("1969-12-31 23:00".into()), Value::Float(-3600.0)],
        ]
    );
    // In WHERE, over the rows the scan returns; on values and text.
    assert_eq!(sql(&db, "SELECT id FROM snaps WHERE EXTRACT(year FROM at) = 2026"), vec![vec![Value::Int(1)]]);
    assert_eq!(sql(&db, "SELECT id FROM snaps WHERE strftime('%H', at) = '23'"), vec![vec![Value::Int(3)]]);
    assert_eq!(sql(&db, "SELECT date_trunc('month', at) FROM snaps WHERE id = 1"), vec![vec![ts("2026-11-01")]]);
    // A WITH of groups by day, read by a window: the trend query.
    let rows = sql(
        &db,
        "WITH daily AS (SELECT date_trunc('day', at) AS day, AVG(total) AS px FROM snaps WHERE at IS NOT NULL GROUP BY day) \
         SELECT day, px, AVG(px) OVER (ORDER BY day ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) AS ma7 FROM daily ORDER BY day",
    );
    assert_eq!(rows, vec![vec![ts("1969-12-31"), Value::Float(30.0), Value::Float(30.0)], vec![ts("2026-11-05"), Value::Float(10.0), Value::Float(20.0)]]);

    // date_bin: buckets of a fixed width from 1970, or from an origin; a
    // day's bucket is date_trunc's day.
    let rows = sql(&db, "SELECT id, date_bin('5 minutes', at), date_bin('15m', at, '2026-11-05T07:20:00Z'), date_bin('1 day', at) = date_trunc('day', at), date_bin('250ms', at) FROM snaps ORDER BY id");
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), ts("2026-11-05T07:30:00Z"), ts("2026-11-05T07:20:00Z"), Value::Bool(true), ts("2026-11-05T07:31:52.25Z")],
            vec![Value::Int(2), Value::Null, Value::Null, Value::Null, Value::Null],
            vec![Value::Int(3), ts("1969-12-31T23:00:00Z"), ts("1969-12-31T22:50:00Z"), Value::Bool(true), ts("1969-12-31T23:00:00Z")],
        ]
    );
    assert_eq!(sql(&db, "SELECT id FROM snaps WHERE date_bin('1 hour', at) = '2026-11-05T07:00:00Z'"), vec![vec![Value::Int(1)]]);
    // time_bucket is date_bin's other name, and takes the width as an INTERVAL too.
    assert_eq!(sql(&db, "SELECT time_bucket('5 minutes', at) = date_bin(INTERVAL '5 minutes', at) FROM snaps WHERE id = 1"), vec![vec![Value::Bool(true)]]);

    // What is wrong, said.
    for (q, says) in [
        ("SELECT date_trunc('fortnight', at) FROM snaps", "no unit \"fortnight\""),
        ("SELECT date_part('century', at) FROM snaps", "no part \"century\""),
        ("SELECT strftime('%Q', at) FROM snaps", "no %Q"),
        ("SELECT date_trunc('day', total) FROM snaps", "takes a TIMESTAMP"),
        ("SELECT date_trunc('day') FROM snaps", "two arguments"),
        ("SELECT to_char(at, 'YYYY') FROM snaps", "strftime"),
        ("SELECT date_trunc('fortnight', at) AS g, COUNT(*) FROM snaps GROUP BY g", "no unit \"fortnight\""),
        ("SELECT date_bin('1 month', at) FROM snaps", "fixed unit"),
        ("SELECT date_bin('1 month', at) AS g, COUNT(*) FROM snaps GROUP BY g", "fixed unit"),
        ("SELECT date_bin('0 minutes', at) FROM snaps", "fixed unit"),
        ("SELECT date_bin('5 minutes', at, 'yesterday') FROM snaps", "not a time"),
        ("SELECT date_bin('5 minutes', total) FROM snaps", "takes a TIMESTAMP"),
        ("SELECT date_bin('5 minutes') FROM snaps", "two or three arguments"),
    ] {
        let e = db.query(q, &Params::new()).err().unwrap_or_else(|| panic!("{q} is an error")).to_string();
        assert!(e.contains(says), "{q}: {e}");
    }
}

/// A day as other databases write it: `CAST(t AS DATE)`, `DATE(t)` and
/// `CURRENT_DATE`, each `date_trunc('day', ...)`, in SELECT, GROUP BY and
/// WHERE, on columns and on values.
#[test]
fn days_as_cast_date_and_current_date() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let now = timestamp::now();
    let today = timestamp::trunc(now, timestamp::Unit::Day).unwrap();
    let t = |micros: i64| format!("'{}'", timestamp::format(micros));
    sql(&db, &format!("INSERT INTO snaps VALUES (1, {}, 10, 'fixed'), (2, {}, 20, 'fixed'), (3, {}, 30, 'auction'), (4, NULL, 40, 'fixed')", t(today + 60_000_000), t(today + 120_000_000), t(today - 86_400_000_000 * 3)));
    let day = |micros: i64| Value::Timestamp(micros);
    assert_eq!(sql(&db, "SELECT id, CAST(at AS DATE), DATE(at) FROM snaps WHERE id IN (1, 4) ORDER BY id"), vec![vec![Value::Int(1), day(today), day(today)], vec![Value::Int(4), Value::Null, Value::Null]]);
    assert_eq!(sql(&db, "SELECT CAST('2026-11-05T07:31:52Z' AS DATE), DATE('2026-11-05 23:59'), CURRENT_DATE"), vec![vec![day(1_793_836_800_000_000), day(1_793_836_800_000_000), day(today)]]);
    let rows = sql(&db, "SELECT CAST(at AS DATE) AS d, COUNT(*) AS n FROM snaps WHERE at IS NOT NULL GROUP BY d ORDER BY n DESC");
    assert_eq!(rows, vec![vec![day(today), Value::Int(2)], vec![day(today - 86_400_000_000 * 3), Value::Int(1)]]);
    assert_eq!(sql(&db, "SELECT id FROM snaps WHERE at >= CURRENT_DATE ORDER BY id"), vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    assert_eq!(sql(&db, "SELECT COUNT(*) FROM snaps WHERE at >= CURRENT_DATE - INTERVAL '7 days'"), vec![vec![Value::Int(3)]]);
    assert_eq!(sql(&db, "SELECT id FROM snaps WHERE CAST(at AS DATE) = CURRENT_DATE - INTERVAL '3 days'"), vec![vec![Value::Int(3)]]);
}
