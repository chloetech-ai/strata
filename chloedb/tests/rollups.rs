//! Rollups ([`chloedb::query::rollup`]): a table of counts and sums per
//! bucket of time, kept equal to a fresh aggregate of its table as rows
//! arrive, late ones too, and across reopening.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chloedb::db::{Db, SqlResult, WriteOp};
use chloedb::query::Params;
use chloedb::storage::DbOptions;
use chloedb::value::Value;
use chloedb::Schema;

const MIN: i64 = 60_000_000;

fn logs(options: &str) -> Schema {
    Schema::from_json(&format!(
        r#"{{"table": "logs", {options} "fields": [
            {{"name": "ts", "type": "timestamp"}}, {{"name": "service", "type": "keyword"}},
            {{"name": "severity", "type": "keyword"}}, {{"name": "latency", "type": "int"}}]}}"#
    ))
    .unwrap()
}

/// A small generator, so that runs repeat.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) % n
    }
}

fn write(db: &Db, rng: &mut Lcg, n: usize, from: i64, to: i64) {
    let ops = (0..n)
        .map(|_| {
            let ts = from + rng.next((to - from) as u64) as i64;
            let service = format!("svc{}", rng.next(4));
            let severity = ["INFO", "WARN", "ERROR"][rng.next(3) as usize];
            let latency = match rng.next(10) {
                0 => Value::Null,
                _ => Value::Int(rng.next(5000) as i64),
            };
            WriteOp::Insert { table: "logs".into(), doc: vec![Value::Timestamp(ts), Value::Str(service), Value::Str(severity.into()), latency] }
        })
        .collect();
    db.write_ops(ops).unwrap();
}

type Groups = BTreeMap<(i64, String), (i64, i64, i64)>;

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &Params::new()).unwrap().rows
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) | Value::Timestamp(n) => *n,
        Value::Float(f) => *f as i64,
        Value::Null => -1,
        v => panic!("{v:?}"),
    }
}

/// Count, sum and greatest latency by 5 minutes and service, from the
/// table itself.
fn fresh(db: &Db) -> Groups {
    let mut g = Groups::new();
    for r in rows(db, "SELECT ts, service, latency FROM logs WHERE severity <> 'INFO' LIMIT 1000000") {
        let b = int(&r[0]).div_euclid(5 * MIN) * 5 * MIN;
        let e = g.entry((b, r[1].as_str().unwrap().to_string())).or_insert((0, 0, -1));
        e.0 += 1;
        if let Value::Int(l) = r[2] {
            e.1 += l;
            e.2 = e.2.max(l);
        }
    }
    g
}

/// The same from the rollup.
fn rolled(db: &Db) -> Groups {
    rows(db, "SELECT bucket, service, count, sum_latency, worst FROM logs_5m LIMIT 1000000")
        .into_iter()
        .map(|r| ((int(&r[0]), r[1].as_str().unwrap().to_string()), (int(&r[2]), int(&r[3]).max(0), int(&r[4]))))
        .collect()
}

const CREATE: &str = "CREATE ROLLUP logs_5m ON logs OPTIONS(lateness = '10 minutes') AS
    SELECT date_bin('5 minutes', ts), service, COUNT(*), SUM(latency), MAX(latency) AS worst
    WHERE severity <> 'INFO'";

fn refresh(db: &Db, full: bool) -> (i64, i64) {
    let sql = format!("REFRESH ROLLUP logs_5m{}", if full { " FULL" } else { "" });
    let SqlResult::Rows(rs) = db.sql(&sql, &Params::new()).unwrap() else { panic!("rows") };
    assert_eq!(rs.columns[..2], ["buckets_checked", "buckets_folded"]);
    (int(&rs.rows[0][0]), int(&rs.rows[0][1]))
}

#[test]
fn a_rollup_equals_a_fresh_aggregate_after_late_rows_and_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.strata");
    let db = Db::create(&path, logs(""), DbOptions::default()).unwrap();
    let mut rng = Lcg(7);
    let t0 = 1_700_000_000_000_000 / (5 * MIN) * (5 * MIN);
    write(&db, &mut rng, 3000, t0, t0 + 120 * MIN);
    db.sql(CREATE, &Params::new()).unwrap();
    let want = fresh(&db);
    assert!(want.len() > 50, "{}", want.len());
    assert_eq!(rolled(&db), want);
    // Nothing changed: every bucket checked, none folded.
    let (checked, folded) = refresh(&db, true);
    assert_eq!((checked, folded), (24, 0));

    // New rows, and late ones within the lateness: a plain refresh.
    write(&db, &mut rng, 500, t0 + 120 * MIN, t0 + 150 * MIN);
    write(&db, &mut rng, 50, t0 + 141 * MIN, t0 + 150 * MIN);
    db.storage().flush().unwrap();
    let (checked, folded) = refresh(&db, false);
    assert!(checked <= 9 && folded >= 6, "{checked} {folded}");
    assert_eq!(rolled(&db), fresh(&db));

    // Rows later than the lateness: a plain refresh does not look back so
    // far, a full one does.
    write(&db, &mut rng, 40, t0, t0 + 30 * MIN);
    refresh(&db, false);
    assert_ne!(rolled(&db), fresh(&db));
    let (_, folded) = refresh(&db, true);
    assert!((1..=6).contains(&folded), "{folded}");
    assert_eq!(rolled(&db), fresh(&db));

    // The definition is a stored function: it survives reopening.
    drop(db);
    let db = Db::open(&path, DbOptions::default()).unwrap();
    assert_eq!(db.rollups().unwrap().len(), 1);
    write(&db, &mut rng, 100, t0 + 145 * MIN, t0 + 155 * MIN);
    refresh(&db, false);
    assert_eq!(rolled(&db), fresh(&db));

    // An hour from twelve buckets of five minutes.
    let hourly = rows(&db, "SELECT date_trunc('hour', bucket) AS h, SUM(count) FROM logs_5m GROUP BY h ORDER BY h");
    let direct = rows(&db, "SELECT date_trunc('hour', ts) AS h, COUNT(*) FROM logs WHERE severity <> 'INFO' GROUP BY h ORDER BY h");
    assert_eq!(hourly.iter().map(|r| (int(&r[0]), int(&r[1]))).collect::<Vec<_>>(), direct.iter().map(|r| (int(&r[0]), int(&r[1]))).collect::<Vec<_>>());

    db.sql("DROP ROLLUP logs_5m", &Params::new()).unwrap();
    assert!(db.rollups().unwrap().is_empty());
    assert!(db.query("SELECT COUNT(*) FROM logs_5m", &Params::new()).is_err());
    db.sql("DROP ROLLUP IF EXISTS logs_5m", &Params::new()).unwrap();
}

#[test]
fn a_rollup_keeps_buckets_its_table_no_longer_has() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("r.strata"), logs(r#""retain": "1 hours", "retain_by": "ts","#), DbOptions::default()).unwrap();
    let now = chloedb::timestamp::now();
    let mut rng = Lcg(3);
    write(&db, &mut rng, 600, now - 90 * MIN, now);
    db.sql(CREATE, &Params::new()).unwrap();
    let before = rolled(&db);
    // Rows past the retention go; the buckets they began to leave stay as
    // the rollup folded them, and the newer ones still follow the table.
    let frozen = |g: &Groups| -> Groups { g.iter().filter(|(k, _)| k.0 < now - 55 * MIN).map(|(k, v)| (k.clone(), *v)).collect() };
    let lines = |db: &Db| int(&rows(db, "SELECT COUNT(*) FROM logs")[0][0]);
    let kept = lines(&db);
    db.optimize().unwrap();
    // Asked of the table's rows, not of the groups the rollup froze: under
    // load the clock moves on between the fold and the merge.
    assert!(lines(&db) < kept, "the retention dropped rows");
    write(&db, &mut rng, 100, now - 20 * MIN, now);
    refresh(&db, true);
    let after = rolled(&db);
    assert_eq!(frozen(&after), frozen(&before));
    let live = |g: &Groups| -> Groups { g.iter().filter(|(k, _)| k.0 >= now - 50 * MIN).map(|(k, v)| (k.clone(), *v)).collect() };
    assert_eq!(live(&after), live(&fresh(&db)));
}

#[test]
fn the_background_job_keeps_a_rollup_up_to_date() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create(&dir.path().join("r.strata"), logs(""), DbOptions::default()).unwrap());
    let mut rng = Lcg(11);
    let t0 = chloedb::timestamp::now() - 60 * MIN;
    write(&db, &mut rng, 1000, t0, t0 + 30 * MIN);
    db.sql(CREATE, &Params::new()).unwrap();
    db.spawn_rollups(Duration::from_millis(100));
    write(&db, &mut rng, 1000, t0 + 30 * MIN, t0 + 60 * MIN);
    let start = Instant::now();
    while rolled(&db) != fresh(&db) {
        assert!(start.elapsed() < Duration::from_secs(20), "the rollup did not catch up");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn rollups_say_what_is_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("r.strata"), logs(""), DbOptions::default()).unwrap();
    let bad = |sql: &str, says: &str| match db.sql(sql, &Params::new()) {
        Err(e) => assert!(e.to_string().contains(says), "{sql}: {e}"),
        Ok(_) => panic!("{sql} ran"),
    };
    bad("CREATE ROLLUP r ON nope AS SELECT date_bin('1 minute', ts), COUNT(*)", "no table nope");
    bad("CREATE ROLLUP r ON logs AS SELECT date_bin('1 minute', service), COUNT(*)", "not a TIMESTAMP");
    bad("CREATE ROLLUP r ON logs AS SELECT date_bin('1 minute', ts), SUM(service)", "needs a number");
    bad("CREATE ROLLUP logs ON logs AS SELECT date_bin('1 minute', ts), COUNT(*)", "a table is named logs");
    bad("REFRESH ROLLUP r", "no rollup r");
    db.sql("CREATE ROLLUP r ON logs AS SELECT date_bin('1 minute', ts), COUNT(*)", &Params::new()).unwrap();
    bad("CREATE ROLLUP r ON logs AS SELECT date_bin('1 minute', ts), COUNT(*)", "rollup r exists");
    db.sql("CREATE ROLLUP IF NOT EXISTS r ON logs AS SELECT date_bin('1 minute', ts), COUNT(*)", &Params::new()).unwrap();
    // An empty table: nothing to fold.
    assert_eq!(db.refresh_rollup("r", true).unwrap().folded, 0);
}

/// A rollup made before 0.3.3 keys its rows by text and keeps a copy of
/// each: it is refreshed as it was made.
#[test]
fn a_rollup_made_with_text_keys_is_kept_as_made() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("r.strata"), logs(""), DbOptions::default()).unwrap();
    let mut rng = Lcg(5);
    let t0 = 1_700_000_000_000_000 / (5 * MIN) * (5 * MIN);
    write(&db, &mut rng, 800, t0, t0 + 60 * MIN);
    db.sql("CREATE TABLE logs_5m (rollup_key KEYWORD PRIMARY KEY, bucket TIMESTAMP, service KEYWORD, count INT, sum_latency INT, worst INT) OPTIONS(cluster_by = 'bucket')", &Params::new()).unwrap();
    let statement = CREATE.split_whitespace().collect::<Vec<_>>().join(" ");
    db.sql(&format!("CREATE OR REPLACE FUNCTION rollup__logs_5m() RETURNS TEXT AS '{}'", statement.replace('\'', "''")), &Params::new()).unwrap();
    refresh(&db, true);
    assert_eq!(rolled(&db), fresh(&db));
    let keys = rows(&db, "SELECT rollup_key FROM logs_5m LIMIT 1");
    assert!(matches!(keys[0][0], Value::Str(_)), "{keys:?}");
    write(&db, &mut rng, 100, t0 + 50 * MIN, t0 + 70 * MIN);
    refresh(&db, true);
    assert_eq!(rolled(&db), fresh(&db));
}
