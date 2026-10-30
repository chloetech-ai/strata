//! Caps on what one query may return or build: rows (LIMIT plus OFFSET, or
//! every match of a query without a LIMIT) and groups.

use strata::data;
use strata::db::Db;
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::VectorIndexKind;

fn db(dir: &std::path::Path) -> Db {
    let opts = DbOptions { segment_max_docs: 200, ..DbOptions::default() };
    let db = Db::create(&dir.join("l.strata"), data::beir_schema(8, VectorIndexKind::Flat), opts).unwrap();
    db.load(data::synthetic(1000, 8, 5)).unwrap();
    db
}

fn run(db: &Db, sql: &str, opts: &ExecOptions) -> Result<usize, String> {
    db.query_with(sql, &Params::new(), opts).map(|r| r.rows.len()).map_err(|e| e.to_string())
}

#[test]
fn rows_past_the_window_fail() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let capped = ExecOptions { max_rows: Some(100), ..ExecOptions::default() };
    let count = |sql: &str| db.query_with(sql, &Params::new(), &ExecOptions::default()).unwrap().rows[0][0].as_i64().unwrap() as usize;
    let n_recent = count("SELECT COUNT(*) FROM docs WHERE year > 2021");
    assert!((50..=100).contains(&n_recent), "{n_recent}: the test needs a filter matching under 100 rows");
    let ok = [
        ("SELECT id FROM docs LIMIT 100", 100),
        ("SELECT id FROM docs LIMIT 40 OFFSET 60", 40),
        ("SELECT id FROM docs WHERE year > 2021", n_recent),
        ("SELECT id FROM docs ORDER BY rating DESC LIMIT 100", 100),
        // Counts and groups are not rows of the table.
        ("SELECT COUNT(*) FROM docs", 1),
    ];
    for (sql, n) in ok {
        assert_eq!(run(&db, sql, &capped), Ok(n), "{sql}");
    }
    // Ranked without a LIMIT: the matches, up to DEFAULT_K (100).
    let ranked = "SELECT id FROM docs ORDER BY bm25(body, 'data') DESC";
    assert_eq!(run(&db, ranked, &capped), run(&db, ranked, &ExecOptions::default()));
    let too_many = [
        ("SELECT id FROM docs LIMIT 101", "reach row 101"),
        ("SELECT id FROM docs LIMIT 50 OFFSET 60", "reach row 110"),
        ("SELECT id FROM docs ORDER BY bm25(body, 'data') DESC LIMIT 10 OFFSET 95", "reach row 105"),
        ("SELECT id FROM docs", "matches more than 100 rows"),
        ("SELECT id FROM docs WHERE year > 2000", "matches more than 100 rows"),
        ("SELECT id FROM docs ORDER BY rating DESC", "matches more than 100 rows"),
    ];
    for (sql, why) in too_many {
        let e = run(&db, sql, &capped).unwrap_err();
        assert!(e.contains(why), "{sql}: {e}");
        // Without the cap, it answers.
        assert!(run(&db, sql, &ExecOptions::default()).is_ok(), "{sql}");
    }
    // A shard answers its share of LIMIT plus OFFSET, and is held to the
    // same cap.
    let shard = ExecOptions { shard: true, ..capped.clone() };
    assert_eq!(run(&db, "SELECT id FROM docs LIMIT 30 OFFSET 70", &shard), Ok(100));
    assert!(run(&db, "SELECT id FROM docs LIMIT 31 OFFSET 70", &shard).is_err());
}

#[test]
fn groups_past_the_cap_fail() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let years = run(&db, "SELECT year, COUNT(*) FROM docs GROUP BY year", &ExecOptions::default()).unwrap();
    let topics = run(&db, "SELECT topic, COUNT(*) FROM docs GROUP BY topic", &ExecOptions::default()).unwrap();
    assert!(years > 20 && topics > 20, "{years} years, {topics} topics");
    let queries = [
        // A keyword's facet counters.
        "SELECT topic, COUNT(*) FROM docs GROUP BY topic",
        // Document by document.
        "SELECT topic, AVG(rating) FROM docs GROUP BY topic",
        "SELECT year, COUNT(*) FROM docs GROUP BY year",
        "SELECT year, topic, COUNT(*) FROM docs GROUP BY year, topic",
    ];
    for sql in queries {
        let all = run(&db, sql, &ExecOptions::default()).unwrap();
        let fits = ExecOptions { max_groups: Some(all), ..ExecOptions::default() };
        assert_eq!(run(&db, sql, &fits), Ok(all), "{sql}");
        let short = ExecOptions { max_groups: Some(all - 1), ..ExecOptions::default() };
        let e = run(&db, sql, &short).unwrap_err();
        assert!(e.contains(&format!("more than {} groups", all - 1)), "{sql}: {e}");
        // A LIMIT does not lift it: every group is counted before the cut.
        let limited = format!("{sql} LIMIT 5");
        assert!(run(&db, &limited, &short).is_err(), "{limited}");
        // Nor does shard mode, which returns every group.
        let shard = ExecOptions { shard: true, ..short.clone() };
        assert!(run(&db, sql, &shard).is_err(), "{sql} on a shard");
    }
    // A filter that leaves fewer groups fits.
    let short = ExecOptions { max_groups: Some(10), ..ExecOptions::default() };
    assert!(run(&db, "SELECT year, COUNT(*) FROM docs WHERE year BETWEEN 2000 AND 2009 GROUP BY year", &short).is_ok());
}
