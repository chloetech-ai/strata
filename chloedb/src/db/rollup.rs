//! Rollups kept up to date ([`crate::query::rollup`]): `CREATE ROLLUP`,
//! `REFRESH ROLLUP`, `DROP ROLLUP`, and the background job that refreshes
//! them.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use super::{Db, SqlResult, WriteOp};
use crate::error::{Error, Result};
use crate::query::exec::{ExecOptions, ExecStats, ResultSet};
use crate::query::rollup::{self, Refresh, Rollup, Statement, KEY};
use crate::query::{sql as qsql, Params};
use crate::schema::Schema;
use crate::value::Value;

/// What one refresh of a rollup did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RefreshStats {
    /// Buckets whose counts it compared with the table's.
    pub checked: u64,
    /// Buckets it folded again, their counts having changed.
    pub folded: u64,
    /// Rows of the rollup it wrote or deleted.
    pub rows_written: u64,
    pub seconds: f64,
}

impl RefreshStats {
    fn result_set(&self) -> ResultSet {
        let columns = ["buckets_checked", "buckets_folded", "rows_written", "seconds"].map(String::from).to_vec();
        let rows = vec![vec![Value::Int(self.checked as i64), Value::Int(self.folded as i64), Value::Int(self.rows_written as i64), Value::Float(self.seconds)]];
        ResultSet { columns, rows, keys: Vec::new(), scores: Vec::new(), parts: None, stats: ExecStats::default() }
    }
}

/// Each rollup's lock and how its refreshes went, by its name in lower
/// case.
#[derive(Default)]
pub(super) struct Rollups {
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    health: Mutex<HashMap<String, RollupHealth>>,
}

/// How a rollup's refreshes went, since this process opened the database
/// ([`Db::rollup_health`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RollupHealth {
    /// When its last refresh that did not fail ended, in microseconds.
    pub last_refresh: Option<i64>,
    /// When its last refresh failed, and why, unless one has since done
    /// well.
    pub last_error: Option<(i64, String)>,
    /// Refreshes that failed in a row.
    pub failures: u64,
}

/// How often the background job checks every bucket, not only the
/// newest: every this many refreshes.
const SWEEP_EVERY: u64 = 60;

/// Buckets one query counts or folds at most.
const SPAN: i64 = 288;

/// Buckets apart that two stale buckets may be and still be folded by one
/// query (the rows between them are read for nothing).
const GAP: i64 = 12;

fn all_rows(opts: &ExecOptions) -> ExecOptions {
    ExecOptions { max_rows: None, max_groups: None, ..opts.clone() }
}

fn window(from: i64, to: i64) -> Params {
    Params::new().set("rollup_from", Value::Timestamp(from)).set("rollup_to", Value::Timestamp(to))
}

fn time_of(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Timestamp(t) | Value::Int(t) => Some(*t),
        Value::Float(f) => Some(*f as i64),
        _ => None,
    }
}

fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Int(n) | Value::Timestamp(n)) => *n,
        Some(Value::Float(f)) => *f as i64,
        _ => 0,
    }
}

/// `v` as a value of a column of SQL type `ty`.
fn fit(v: Value, ty: &str) -> Value {
    match (v, ty) {
        (Value::Float(f), "INT") if f.fract() == 0.0 => Value::Int(f as i64),
        (Value::Int(n) | Value::Timestamp(n), "TIMESTAMP") => Value::Timestamp(n),
        (Value::Float(f), "TIMESTAMP") => Value::Timestamp(f as i64),
        (Value::Int(n), "FLOAT") => Value::Float(n as f64),
        (Value::Int(n), "BOOL") => Value::Bool(n != 0),
        (v, _) => v,
    }
}

/// Groups a bucket may hold: a row's key is its bucket's number times
/// this, plus its place among the bucket's groups.
const GROUPS: i64 = 1 << 24;

/// The rollup key of the row of bucket `bucket` and group `group`, in a
/// rollup made before 0.3.3, whose key is text.
fn key_of(bucket: i64, group: &[Value]) -> String {
    let mut k = bucket.to_string();
    for v in group {
        k.push('\u{1f}');
        match v {
            Value::Null => k.push('\u{0}'),
            Value::Str(s) => k.push_str(s),
            Value::Int(n) | Value::Timestamp(n) => k.push_str(&n.to_string()),
            Value::Bool(b) => k.push_str(if *b { "1" } else { "0" }),
            Value::Float(f) => k.push_str(&f.to_string()),
            other => k.push_str(&format!("{other:?}")),
        }
    }
    k
}

impl Db {
    /// A `CREATE ROLLUP`, `REFRESH ROLLUP` or `DROP ROLLUP` statement.
    pub(super) fn rollup_statement(&self, st: Statement, opts: &ExecOptions) -> Result<SqlResult> {
        match st {
            Statement::Create { rollup: r, if_not_exists } => match self.create_rollup(&r, if_not_exists, opts)? {
                true => self.refresh(&r, Refresh::Recent, opts).map(|s| SqlResult::Rows(s.result_set())),
                false => Ok(SqlResult::Written(0)),
            },
            Statement::Refresh { name, mode } => {
                let r = self.rollup(&name)?.ok_or_else(|| Error::Query(format!("no rollup {name}")))?;
                self.refresh(&r, mode, opts).map(|s| SqlResult::Rows(s.result_set()))
            }
            Statement::Drop { name, if_exists } => {
                let Some(r) = self.rollup(&name)? else {
                    return match if_exists {
                        true => Ok(SqlResult::Written(0)),
                        false => Err(Error::Query(format!("no rollup {name}"))),
                    };
                };
                self.sql_with(&format!("DROP FUNCTION {}", rollup::function_name(&r.name)), &Params::new(), opts)?;
                if self.storage.table(&r.name).is_some() {
                    self.drop_tables(&[&r.name], Default::default())?;
                }
                self.rollups.health.lock().unwrap_or_else(|e| e.into_inner()).remove(&r.name.to_ascii_lowercase());
                Ok(SqlResult::Written(0))
            }
        }
    }

    /// Makes rollup `r`'s table and keeps its definition, without folding
    /// any rows yet: whether it made it (not when `if_not_exists` and it
    /// exists). `CREATE ROLLUP` then folds the table's rows at once; a
    /// server leaves that to its refresher, which a long table can keep
    /// busy for minutes.
    pub fn create_rollup(&self, r: &Rollup, if_not_exists: bool, opts: &ExecOptions) -> Result<bool> {
        let base = self.table(&r.base)?;
        if self.functions()?.current(&rollup::function_name(&r.name)).is_some() {
            return match if_not_exists {
                true => Ok(false),
                false => Err(Error::Query(format!("rollup {} exists: DROP ROLLUP {} first", r.name, r.name))),
            };
        }
        if self.storage.table(&r.name).is_some() {
            return Err(Error::Query(format!("a table is named {}: a rollup is a table of its own", r.name)));
        }
        if base.table.starts_with('_') {
            return Err(Error::Query(format!("{} is a system table", base.table)));
        }
        self.sql_with(&r.create_table_sql(&base)?, &Params::new(), opts)?;
        // Kept with the mark of its layout, which 0.3.2 refuses by name,
        // and, when it counts a field's values, the mark 0.3.3 refuses.
        let mut statement = rollup::marked(&r.statement);
        if r.counts_fields() {
            statement = rollup::with_mark(&statement, rollup::COUNTS_MARK);
        }
        let kept = Rollup { statement, ..r.clone() };
        if let Err(e) = self.sql_with(&rollup::function_sql(&kept), &Params::new(), opts) {
            let _ = self.drop_tables(&[&r.name], Default::default());
            return Err(e);
        }
        Ok(true)
    }

    /// The rollups this database keeps ([`crate::query::rollup`]).
    pub fn rollups(&self) -> Result<Vec<Rollup>> {
        self.rollups_each()?.into_iter().map(|(_, r)| r).collect()
    }

    /// Each rollup's name and definition, or why its definition does not
    /// read (one made by a later release, say), so that one does not hide
    /// the others.
    fn rollups_each(&self) -> Result<Vec<(String, Result<Rollup>)>> {
        let fs = self.functions()?;
        let mut out = Vec::new();
        for f in fs.currents().filter(|f| f.def.name.to_ascii_lowercase().starts_with(rollup::PREFIX)) {
            let name = f.def.name[rollup::PREFIX.len()..].to_string();
            match rollup::of_function(f) {
                Ok(Some(r)) => out.push((name, Ok(r))),
                Ok(None) => {}
                Err(e) => out.push((name.clone(), Err(Error::Query(format!("rollup {name}: its definition does not read: {e}"))))),
            }
        }
        Ok(out)
    }

    /// The rollups whose definitions read.
    fn rollups_readable(&self) -> Result<Vec<Rollup>> {
        Ok(self.rollups_each()?.into_iter().filter_map(|(_, r)| r.ok()).collect())
    }

    /// Rollup `name`, if there is one.
    pub fn rollup(&self, name: &str) -> Result<Option<Rollup>> {
        match self.functions()?.current(&rollup::function_name(name)) {
            Some(f) => rollup::of_function(f),
            None => Ok(None),
        }
    }

    /// Brings rollup `name` up to date with its table (`REFRESH ROLLUP`).
    ///
    /// A bucket is folded again when the rows of the table it would count
    /// are not as many as its rows of the rollup count: a bucket the
    /// rollup has not yet folded, the newest buckets, which grow as rows
    /// arrive, and an older one that rows reached late. A plain refresh
    /// ([`Refresh::Recent`]) compares the buckets from the clock (or the
    /// newest row, when that is older) less the rollup's lateness, and
    /// those after the newest bucket it has folded up to now; rows dated
    /// in the future are folded too, but do not move that window off the
    /// present. [`Refresh::Full`] compares every bucket, and
    /// [`Refresh::Rebuild`] folds every bucket again whatever its count.
    /// Only buckets that hold rows, in the table or in the rollup, are
    /// compared: a span of time without rows costs one query to step
    /// over. Counting a bucket's rows reads the time column's index, not
    /// the rows, so it takes well under a millisecond. Buckets older than
    /// the table's retention are not compared: the rollup keeps what it
    /// counted before the rows expired.
    ///
    /// One refresh of a rollup runs at a time; another waits for it.
    ///
    /// A changed row that leaves a bucket's count as it was (an UPDATE, a
    /// delete and an insert into one bucket between two refreshes) is not
    /// seen by a plain or a full refresh, ever: only by a rebuild.
    /// Rollups are for tables that rows are added to, such as logs.
    pub fn refresh_rollup(&self, name: &str, full: bool) -> Result<RefreshStats> {
        self.refresh_rollup_with(name, if full { Refresh::Full } else { Refresh::Recent })
    }

    /// [`Db::refresh_rollup`], comparing and folding the buckets `mode`
    /// says.
    pub fn refresh_rollup_with(&self, name: &str, mode: Refresh) -> Result<RefreshStats> {
        let r = self.rollup(name)?.ok_or_else(|| Error::Query(format!("no rollup {name}")))?;
        self.refresh(&r, mode, &self.exec)
    }

    /// How rollup `name`'s refreshes went since the database was opened:
    /// `None` before its first.
    pub fn rollup_health(&self, name: &str) -> Option<RollupHealth> {
        self.rollups.health.lock().unwrap_or_else(|e| e.into_inner()).get(&name.to_ascii_lowercase()).cloned()
    }

    /// Refreshes every rollup, each on its own: one that fails (its table
    /// dropped or changed, say) is reported and leaves the others to go
    /// on. Each rollup's name and what its refresh did, or why it failed.
    pub fn refresh_rollups(&self, mode: Refresh) -> Result<Vec<(String, Result<RefreshStats>)>> {
        let mut out = Vec::new();
        for (name, r) in self.rollups_each()? {
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    let e = Error::Query(e.to_string());
                    self.note_refresh(&name, &Err(Error::Query(e.to_string())));
                    out.push((name, Err(e)));
                    continue;
                }
            };
            let res = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.refresh(&r, mode, &self.exec))) {
                Ok(res) => res,
                Err(_) => {
                    let e = Error::Query(format!("rollup {}: its refresh panicked", r.name));
                    self.note_refresh(&r.name, &Err(Error::Query(e.to_string())));
                    Err(e)
                }
            };
            out.push((r.name, res));
        }
        Ok(out)
    }

    fn note_refresh(&self, name: &str, res: &Result<RefreshStats>) {
        let mut health = self.rollups.health.lock().unwrap_or_else(|e| e.into_inner());
        let h = health.entry(name.to_ascii_lowercase()).or_default();
        let now = crate::timestamp::now();
        match res {
            Ok(_) => {
                h.last_refresh = Some(now);
                h.last_error = None;
                h.failures = 0;
            }
            Err(e) => {
                h.last_error = Some((now, e.to_string()));
                h.failures += 1;
            }
        }
    }

    fn refresh(&self, r: &Rollup, mode: Refresh, opts: &ExecOptions) -> Result<RefreshStats> {
        let lock = self.rollups.locks.lock().unwrap_or_else(|e| e.into_inner()).entry(r.name.to_ascii_lowercase()).or_default().clone();
        let _one = lock.lock().unwrap_or_else(|e| e.into_inner());
        let res = self.refresh_now(r, mode, opts);
        self.note_refresh(&r.name, &res);
        res
    }

    fn refresh_now(&self, r: &Rollup, mode: Refresh, opts: &ExecOptions) -> Result<RefreshStats> {
        let t0 = Instant::now();
        let mut stats = RefreshStats::default();
        let base = self.table(&r.base).map_err(|e| Error::Query(format!("rollup {}: its table {}: {e}; DROP ROLLUP {} if the table is gone", r.name, r.base, r.name)))?;
        let roll = self.table(&r.name).map_err(|e| Error::Query(format!("rollup {}: its own table: {e}; DROP ROLLUP {} and CREATE ROLLUP it again", r.name, r.name)))?;
        let r = &r.fitted(&roll);
        let types = r.check(&base)?;
        if roll.fields.len() != types.len() + 1 {
            return Err(Error::Query(format!(
                "rollup {}: its table has {} columns, and its definition makes {}: DROP ROLLUP {} and CREATE ROLLUP it again",
                r.name,
                roll.fields.len(),
                types.len() + 1,
                r.name
            )));
        }
        let opts = all_rows(opts);
        let one = |sql: &str, p: &Params| -> Result<Option<Value>> { Ok(self.query_with(sql, p, &opts)?.rows.into_iter().next().and_then(|r| r.into_iter().next())) };
        let Some(newest) = time_of(one(&r.edge_sql(true), &Params::new())?.as_ref()) else { return Ok(stats) };
        let now = crate::timestamp::now();
        let bucket = qsql::ident(&r.bucket);
        let folded = time_of(one(&r.folded_sql(), &Params::new().set("rollup_now", Value::Timestamp(now)))?.as_ref());
        let last = r.bucket_of(newest);
        // The window of a plain refresh is the clock's: a row dated in the
        // future (a device's clock gone wrong) is folded, but does not
        // take the window with it.
        let hot = r.bucket_of(newest.min(now).saturating_sub(r.lateness));
        let mut first = match (mode, folded) {
            (Refresh::Recent, Some(m)) => hot.min(m.saturating_add(r.width)),
            _ => match time_of(one(&r.edge_sql(false), &Params::new())?.as_ref()) {
                Some(oldest) => r.bucket_of(oldest),
                None => return Ok(stats),
            },
        };
        // Buckets that rows have begun to leave by the table's retention
        // stay as they were folded.
        if let Some(keep) = &base.retain {
            let cutoff = now.saturating_sub(keep.seconds.saturating_mul(1_000_000));
            first = first.max(r.bucket_of(cutoff).saturating_add(r.width));
        }
        let end = last.saturating_add(r.width);
        let held_sql = format!("SELECT {bucket}, SUM({}) FROM {} WHERE {bucket} >= :rollup_from AND {bucket} < :rollup_to GROUP BY {bucket}", qsql::ident(r.count_column()), qsql::ident(&r.name));
        let pairs = |sql: &str, from: i64, to: i64| -> Result<HashMap<i64, i64>> { Ok(self.query_with(sql, &window(from, to), &opts)?.rows.into_iter().filter_map(|row| Some((time_of(row.first())?, int_of(row.get(1))))).collect()) };
        // The rollup's count of each bucket it holds, and the table's, a
        // day of buckets to a query at most; only buckets either one holds
        // are compared, and a span neither holds is stepped over.
        let mut counts: HashMap<i64, i64> = HashMap::new();
        let mut stale: Vec<i64> = Vec::new();
        let mut from = first;
        while from < end {
            let to = from.saturating_add(SPAN.saturating_mul(r.width)).min(end);
            let have = pairs(&r.bucket_counts_sql(), from, to)?;
            let held = pairs(&held_sql, from, to)?;
            let buckets: BTreeSet<i64> = have.keys().chain(held.keys()).copied().collect();
            for b in &buckets {
                stats.checked += 1;
                let (n, m) = (have.get(b).copied().unwrap_or(0), held.get(b).copied().unwrap_or(0));
                if n != m || mode == Refresh::Rebuild {
                    stale.push(*b);
                }
            }
            let empty = buckets.is_empty();
            counts.extend(held);
            from = match empty {
                false => to,
                // Nothing here: on to the next bucket either side holds.
                true => {
                    let p = Params::new().set("rollup_from", Value::Timestamp(to));
                    let next = [time_of(one(&r.next_sql(), &p)?.as_ref()), time_of(one(&r.next_folded_sql(), &p)?.as_ref())].into_iter().flatten().min();
                    match next {
                        Some(t) => r.bucket_of(t).max(to),
                        None => break,
                    }
                }
            };
        }
        // Folded in runs of nearby buckets, a query a run.
        let mut i = 0;
        while i < stale.len() {
            let mut j = i + 1;
            while j < stale.len() && stale[j].saturating_sub(stale[i]) < SPAN.saturating_mul(r.width) && stale[j].saturating_sub(stale[j - 1]) <= GAP.saturating_mul(r.width) {
                j += 1;
            }
            let run = &stale[i..j];
            stats.rows_written += self.fold(r, &roll, &types, run, &counts, &opts)?;
            stats.folded += run.len() as u64;
            i = j;
        }
        stats.seconds = t0.elapsed().as_secs_f64();
        Ok(stats)
    }

    /// Folds buckets `run` of rollup `r` again, sorted and within a day of
    /// each other, by one query of the table: for each bucket, in one
    /// transaction, a row for each group it holds now and none for the
    /// groups it no longer holds. `counts` is the rollup's count of each
    /// bucket: one it holds no rows of has none to delete.
    fn fold(&self, r: &Rollup, roll: &Schema, types: &[(String, &'static str)], run: &[i64], counts: &HashMap<i64, i64>, opts: &ExecOptions) -> Result<u64> {
        let (from, to) = (run[0], run[run.len() - 1] + r.width);
        let wanted: HashSet<i64> = run.iter().copied().collect();
        let keys = r.keys.len();
        // A rollup made before 0.3.3 keys its rows by text; one made since,
        // by number: each fold writes a bucket's rows whole, so a row's key
        // need only be unique, not the same from one fold to the next.
        let text_keys = roll.field(KEY).is_some_and(|f| matches!(f.ty, crate::schema::FieldType::Keyword));
        let mut rows_of: HashMap<i64, Vec<Vec<Value>>> = HashMap::new();
        for mut row in self.query_with(&r.fold_range_sql(), &window(from, to), opts)?.rows {
            let Some(b) = time_of(row.first()) else { continue };
            if wanted.contains(&b) {
                row.remove(0);
                rows_of.entry(b).or_default().push(row);
            }
        }
        let mut by_bucket: HashMap<i64, Vec<WriteOp>> = HashMap::new();
        let mut wrote: HashSet<String> = HashSet::new();
        for (b, mut rows) in rows_of {
            if rows.len() as i64 > GROUPS {
                return Err(Error::Query(format!("rollup {}: a bucket holds {} groups, more than {GROUPS}", r.name, rows.len())));
            }
            // A number key is the bucket's number times GROUPS: a bucket
            // too far from 1970 for that to fit in 64 bits (a time stored
            // in nanoseconds, say) is left out, rather than wrap onto the
            // key of another bucket.
            let at = b.div_euclid(r.width).checked_mul(GROUPS).filter(|k| k.checked_add(GROUPS).is_some());
            if at.is_none() && !text_keys {
                tracing::warn!(rollup = %r.name, bucket = b, "a rollup's bucket is too far from 1970 for its keys; its rows are not folded");
                continue;
            }
            // In the groups' order, so that a fold of the same rows writes
            // the same keys.
            rows.sort_by_cached_key(|row| key_of(b, &row[..keys]));
            for (i, row) in rows.into_iter().enumerate() {
                let key = match text_keys {
                    true => Value::Str(key_of(b, &row[..keys])),
                    false => Value::Int(at.unwrap_or(0) + i as i64),
                };
                let mut doc = Vec::with_capacity(roll.fields.len());
                doc.push(key.clone());
                doc.push(Value::Timestamp(b));
                // types[0] is the bucket's; the groups and aggregates follow.
                doc.extend(row.into_iter().zip(&types[1..]).map(|(v, (_, ty))| fit(v, ty)));
                wrote.insert(format!("{key:?}"));
                by_bucket.entry(b).or_default().push(WriteOp::Upsert { table: r.name.clone(), doc });
            }
        }
        if run.iter().any(|b| counts.get(b).copied().unwrap_or(0) != 0) {
            let had = format!("SELECT {KEY}, {} FROM {} WHERE {} >= :rollup_from AND {} < :rollup_to", qsql::ident(&r.bucket), qsql::ident(&r.name), qsql::ident(&r.bucket), qsql::ident(&r.bucket));
            for row in self.query_with(&had, &window(from, to), opts)?.rows {
                let (Some(k), Some(b)) = (row.first(), time_of(row.get(1))) else { continue };
                if wanted.contains(&b) && !wrote.contains(&format!("{k:?}")) {
                    by_bucket.entry(b).or_default().push(WriteOp::Delete { table: r.name.clone(), key: k.clone() });
                }
            }
        }
        // Whole buckets to a transaction, a few thousand rows each.
        let mut written = 0;
        let mut ops: Vec<WriteOp> = Vec::new();
        for b in run {
            let Some(mut more) = by_bucket.remove(b) else { continue };
            if !ops.is_empty() && ops.len() + more.len() > super::GROUP_OPS {
                written += self.write_ops(std::mem::take(&mut ops))?;
            }
            ops.append(&mut more);
        }
        written += self.write_ops(ops)?;
        Ok(written)
    }

    /// Refreshes every rollup in the background from now on, every
    /// `every`, and every bucket of each every sixty refreshes
    /// ([`Db::refresh_rollup`]). The thread ends once the database is
    /// dropped.
    pub fn spawn_rollups(self: &Arc<Self>, every: Duration) {
        let db: Weak<Db> = Arc::downgrade(self);
        let spawned = std::thread::Builder::new().name("chloedb-rollups".into()).spawn(move || {
            let mut tick: u64 = 0;
            loop {
                let t0 = Instant::now();
                while t0.elapsed() < every {
                    std::thread::sleep(Duration::from_millis(200).min(every));
                    if db.strong_count() == 0 {
                        return;
                    }
                }
                let Some(db) = db.upgrade() else { return };
                let mode = if tick % SWEEP_EVERY == SWEEP_EVERY - 1 { Refresh::Full } else { Refresh::Recent };
                tick += 1;
                match db.refresh_rollups(mode) {
                    Ok(done) => {
                        for (name, res) in done {
                            match res {
                                Ok(s) if s.folded > 0 => tracing::debug!(rollup = %name, checked = s.checked, folded = s.folded, rows = s.rows_written, secs = s.seconds, "refreshed a rollup"),
                                Ok(_) => {}
                                // Said once, at its first failure in a row.
                                Err(e) if db.rollup_health(&name).is_some_and(|h| h.failures == 1) => tracing::warn!(rollup = %name, error = %e, "a rollup's refresh failed; the next one tries again"),
                                Err(_) => {}
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "could not read the rollups"),
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the rollups' thread");
        }
    }
}

/// Whether `text`, a condition, names column `name`, as a word of its
/// own.
fn names_column(text: &str, name: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w.eq_ignore_ascii_case(name))
}

impl Db {
    /// Before a `DROP TABLE` of `names`: refuses to drop a table that a
    /// rollup reads unless the rollup's table goes too, and returns the
    /// rollups whose own tables go, whose definitions go with them
    /// ([`Db::forget_rollups`]).
    pub(super) fn rollups_dropped_with(&self, names: &[&str]) -> Result<Vec<Rollup>> {
        let dropped = |t: &str| names.iter().any(|n| n.eq_ignore_ascii_case(t));
        let rollups = self.rollups_readable()?;
        if let Some(r) = rollups.iter().find(|r| dropped(&r.base) && !dropped(&r.name)) {
            let readers: Vec<&str> = rollups.iter().filter(|o| o.base.eq_ignore_ascii_case(&r.base) && !dropped(&o.name)).map(|o| o.name.as_str()).collect();
            let list = readers.join(", ");
            return Err(Error::Query(format!(
                "table {} is read by rollup{} {list}: DROP ROLLUP {} first (or DROP TABLE {}, {list} to drop both)",
                r.base,
                if readers.len() == 1 { "" } else { "s" },
                readers[0],
                r.base
            )));
        }
        Ok(rollups.into_iter().filter(|r| dropped(&r.name)).collect())
    }

    /// Drops the definitions of rollups `gone`, whose tables were dropped.
    pub(super) fn forget_rollups(&self, gone: &[Rollup]) -> Result<()> {
        for r in gone {
            self.sql_with(&format!("DROP FUNCTION IF EXISTS {}", rollup::function_name(&r.name)), &Params::new(), &self.exec)?;
            self.rollups.health.lock().unwrap_or_else(|e| e.into_inner()).remove(&r.name.to_ascii_lowercase());
        }
        Ok(())
    }

    /// Before an `ALTER TABLE` of `table`: refuses to change a rollup's own
    /// table, whose columns its definition makes, and to drop or rename a
    /// column a rollup reads.
    pub(super) fn check_rollups_alter(&self, table: &str, alters: &[crate::schema::Alter]) -> Result<()> {
        use crate::schema::Alter;
        for r in self.rollups_readable()? {
            if r.name.eq_ignore_ascii_case(table) {
                return Err(Error::Query(format!("{table} is a rollup's table, whose columns its definition makes: DROP ROLLUP {table} and CREATE ROLLUP it again to change it")));
            }
            if !r.base.eq_ignore_ascii_case(table) {
                continue;
            }
            for a in alters {
                let (Alter::DropField(c) | Alter::RenameField { from: c, .. }) = a else { continue };
                let read = r.fields().iter().any(|f| f.eq_ignore_ascii_case(c)) || r.filter.as_deref().is_some_and(|f| names_column(f, c));
                if read {
                    return Err(Error::Query(format!("rollup {} reads {table}.{c}: DROP ROLLUP {} first, then CREATE ROLLUP it again after the change", r.name, r.name)));
                }
            }
        }
        Ok(())
    }
}
