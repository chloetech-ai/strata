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

fn refresh_as(db: &Db, how: &str) -> (i64, i64) {
    let SqlResult::Rows(rs) = db.sql(&format!("REFRESH ROLLUP logs_5m {how}"), &Params::new()).unwrap() else { panic!("rows") };
    (int(&rs.rows[0][0]), int(&rs.rows[0][1]))
}

fn insert(db: &Db, ts: i64, service: &str, latency: i64) {
    let doc = vec![Value::Timestamp(ts), Value::Str(service.into()), Value::Str("WARN".into()), Value::Int(latency)];
    db.write_ops(vec![WriteOp::Insert { table: "logs".into(), doc }]).unwrap();
}

/// Audit 4, X-A2 and X-B4: the window of a plain refresh is the clock's,
/// not the newest row's, and a refresh compares only the buckets that
/// hold rows, so one row far in the future or at 1970 costs nothing.
#[test]
fn a_row_dated_far_off_does_not_stall_the_present() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("r.strata"), logs(""), DbOptions::default()).unwrap();
    let now = chloedb::timestamp::now();
    let mut rng = Lcg(13);
    write(&db, &mut rng, 500, now - 60 * MIN, now - 5 * MIN);
    db.sql(CREATE, &Params::new()).unwrap();
    // A device whose clock reads ten years on, and one that sent time 0.
    let future = now + 10 * 365 * 24 * 60 * MIN;
    insert(&db, future, "svc0", 7);
    insert(&db, 0, "svc1", 9);
    let t = Instant::now();
    let (checked, _) = refresh_as(&db, "FULL");
    assert!(checked < 40, "a full refresh compared {checked} buckets");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert_eq!(rolled(&db), fresh(&db));
    // Rows of now are folded by a plain refresh, though the rollup holds a
    // bucket ten years on.
    write(&db, &mut rng, 200, now - 4 * MIN, now);
    let (checked, folded) = refresh(&db, false);
    assert!(folded >= 1 && checked < 20, "{checked} {folded}");
    assert_eq!(rolled(&db), fresh(&db));
    // And so are more rows in the future.
    insert(&db, future + MIN, "svc0", 1);
    refresh(&db, false);
    assert_eq!(rolled(&db), fresh(&db));
}

/// Audit 4, X-B6: `AVG(f)` keeps the count of `f`'s values, so that its
/// mean skips NULLs as `AVG` does; the definition is marked for 0.3.3.
#[test]
fn a_rollups_mean_skips_nulls() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("r.strata"), logs(""), DbOptions::default()).unwrap();
    let mut rng = Lcg(17);
    let t0 = 1_700_000_000_000_000 / (5 * MIN) * (5 * MIN);
    write(&db, &mut rng, 2000, t0, t0 + 60 * MIN);
    db.sql("CREATE ROLLUP lat_5m ON logs AS SELECT date_bin('5 minutes', ts), service, AVG(latency), COUNT(severity)", &Params::new()).unwrap();
    let body = rows(&db, "SELECT rollup__lat_5m()");
    assert!(format!("{body:?}").contains("needs_chloedb_0_3_4"), "{body:?}");
    let mean = |sql: &str| -> BTreeMap<(i64, String), (i64, i64)> {
        rows(&db, sql).into_iter().map(|r| ((int(&r[0]), r[1].as_str().unwrap().to_string()), ((r[2].as_f64().unwrap() * 1000.0).round() as i64, int(&r[3])))).collect()
    };
    let rolled = mean("SELECT date_trunc('hour', bucket) AS h, service, SUM(sum_latency) / SUM(count_latency), SUM(count_severity) FROM lat_5m GROUP BY h, service");
    let direct = mean("SELECT date_trunc('hour', ts) AS h, service, AVG(latency), COUNT(severity) FROM logs GROUP BY h, service");
    assert!(rolled.len() >= 4, "{}", rolled.len());
    assert_eq!(rolled, direct);
}

/// Audit 4, X-A1 and X-B5: one rollup whose table went leaves the others
/// to be refreshed, and says why; the statements that would break a
/// rollup are refused.
#[test]
fn one_broken_rollup_leaves_the_others_and_drops_that_break_one_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("r.strata"), logs(""), DbOptions::default()).unwrap();
    let mut rng = Lcg(19);
    let t0 = 1_700_000_000_000_000 / (5 * MIN) * (5 * MIN);
    write(&db, &mut rng, 500, t0, t0 + 30 * MIN);
    db.sql("CREATE TABLE audit (ts TIMESTAMP, who KEYWORD)", &Params::new()).unwrap();
    db.sql("INSERT INTO audit (ts, who) VALUES ('2026-11-01T10:00:00Z', 'ann')", &Params::new()).unwrap();
    db.sql("CREATE ROLLUP a_1m ON audit AS SELECT date_bin('1 minute', ts), who, COUNT(*)", &Params::new()).unwrap();
    db.sql(CREATE, &Params::new()).unwrap();
    let bad = |sql: &str, says: &str| match db.sql(sql, &Params::new()) {
        Err(e) => assert!(e.to_string().contains(says), "{sql}: {e}"),
        Ok(_) => panic!("{sql} ran"),
    };
    bad("DROP TABLE logs", "read by rollup logs_5m");
    bad("ALTER TABLE logs DROP COLUMN latency", "rollup logs_5m reads logs.latency");
    bad("ALTER TABLE logs RENAME COLUMN severity TO level", "rollup logs_5m reads logs.severity");
    bad("ALTER TABLE logs_5m ADD COLUMN note KEYWORD", "a rollup's table");
    db.sql("ALTER TABLE audit ADD COLUMN note KEYWORD", &Params::new()).unwrap();
    // As an older release lets it: the table of a_1m goes, its definition stays.
    db.drop_tables(&["audit"], Default::default()).unwrap();
    write(&db, &mut rng, 100, t0 + 10 * MIN, t0 + 35 * MIN);
    let done = db.refresh_rollups(chloedb::query::rollup::Refresh::Full).unwrap();
    assert_eq!(done.len(), 2);
    for (name, res) in &done {
        match name.as_str() {
            "a_1m" => assert!(res.as_ref().unwrap_err().to_string().contains("DROP ROLLUP a_1m"), "{res:?}"),
            _ => assert!(res.as_ref().unwrap().folded > 0, "{res:?}"),
        }
    }
    assert_eq!(rolled(&db), fresh(&db));
    let h = db.rollup_health("a_1m").unwrap();
    assert!(h.last_error.is_some() && h.last_refresh.is_some() && h.failures == 1, "{h:?}");
    assert!(db.rollup_health("logs_5m").unwrap().last_error.is_none());
    // DROP TABLE of a rollup's own table drops its definition too, and a
    // table may go with its rollups in one statement.
    db.sql("DROP TABLE a_1m", &Params::new()).unwrap();
    db.sql("CREATE TABLE other (ts TIMESTAMP)", &Params::new()).unwrap();
    db.sql("CREATE ROLLUP o_1m ON other AS SELECT date_bin('1 minute', ts), COUNT(*)", &Params::new()).unwrap();
    db.sql("DROP TABLE other, o_1m", &Params::new()).unwrap();
    assert_eq!(db.rollups().unwrap().iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["logs_5m"]);
    db.sql("DROP TABLE logs_5m", &Params::new()).unwrap();
    assert!(db.rollups().unwrap().is_empty());
}

/// Audit 4, X-B8: an UPDATE keeps a bucket's count, so no plain or full
/// refresh sees it; a rebuild does.
#[test]
fn a_rebuild_sees_an_update() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(
        r#"{"table": "logs", "primary_key": "id", "fields": [{"name": "id", "type": "int"},
            {"name": "ts", "type": "timestamp"}, {"name": "service", "type": "keyword"},
            {"name": "severity", "type": "keyword"}, {"name": "latency", "type": "int"}]}"#,
    )
    .unwrap();
    let db = Db::create(&dir.path().join("r.strata"), schema, DbOptions::default()).unwrap();
    let t0 = 1_700_000_000_000_000 / (5 * MIN) * (5 * MIN);
    let ops = (0..300).map(|i| WriteOp::Insert { table: "logs".into(), doc: vec![Value::Int(i), Value::Timestamp(t0 + i * 10_000_000), Value::Str(format!("svc{}", i % 3)), Value::Str("WARN".into()), Value::Int(1000 + i)] }).collect();
    db.write_ops(ops).unwrap();
    db.sql(CREATE, &Params::new()).unwrap();
    assert_eq!(rolled(&db), fresh(&db));
    db.sql("UPDATE logs SET latency = 1 WHERE service = 'svc0'", &Params::new()).unwrap();
    refresh(&db, true);
    assert_ne!(rolled(&db), fresh(&db), "a full refresh compares counts only");
    let (checked, folded) = refresh_as(&db, "REBUILD");
    assert_eq!(checked, folded);
    assert_eq!(rolled(&db), fresh(&db));
}

/// Audit 4, X-A3 and X-B7: refreshes of one rollup at once, while rows of
/// new groups arrive, leave it equal to its table: one runs at a time.
#[test]
fn refreshes_at_once_take_turns() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create(&dir.path().join("r.strata"), logs(""), DbOptions::default()).unwrap());
    let t0 = 1_700_000_000_000_000 / (5 * MIN) * (5 * MIN);
    let mut rng = Lcg(23);
    write(&db, &mut rng, 300, t0, t0 + 10 * MIN);
    db.sql(CREATE, &Params::new()).unwrap();
    let workers: Vec<_> = (0..4)
        .map(|w| {
            let db = db.clone();
            std::thread::spawn(move || {
                for i in 0..15 {
                    if w == 0 {
                        insert(&db, t0 + (i % 10) * MIN, &format!("new{i}"), i);
                    }
                    db.refresh_rollup("logs_5m", true).unwrap();
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    refresh(&db, true);
    let fresh = fresh(&db);
    assert!(fresh.keys().any(|k| k.1.starts_with("new")));
    assert_eq!(rolled(&db), fresh);
}
