//! Rollups kept up to date ([`crate::query::rollup`]): `CREATE ROLLUP`,
//! `REFRESH ROLLUP`, `DROP ROLLUP`, and the background job that refreshes
//! them.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use super::{Db, SqlResult, WriteOp};
use crate::error::{Error, Result};
use crate::query::exec::{ExecOptions, ExecStats, ResultSet};
use crate::query::rollup::{self, Rollup, Statement, KEY};
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
                true => self.refresh(&r, false, opts).map(|s| SqlResult::Rows(s.result_set())),
                false => Ok(SqlResult::Written(0)),
            },
            Statement::Refresh { name, full } => {
                let r = self.rollup(&name)?.ok_or_else(|| Error::Query(format!("no rollup {name}")))?;
                self.refresh(&r, full, opts).map(|s| SqlResult::Rows(s.result_set()))
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
        // Kept with the mark of its layout, which 0.3.2 refuses by name.
        let kept = Rollup { statement: rollup::marked(&r.statement), ..r.clone() };
        if let Err(e) = self.sql_with(&rollup::function_sql(&kept), &Params::new(), opts) {
            let _ = self.drop_tables(&[&r.name], Default::default());
            return Err(e);
        }
        Ok(true)
    }

    /// The rollups this database keeps ([`crate::query::rollup`]).
    pub fn rollups(&self) -> Result<Vec<Rollup>> {
        let fs = self.functions()?;
        let mut out = Vec::new();
        for f in fs.currents().filter(|f| f.def.name.to_ascii_lowercase().starts_with(rollup::PREFIX)) {
            if let Some(r) = rollup::of_function(f)? {
                out.push(r);
            }
        }
        Ok(out)
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
    /// arrive, and an older one that rows reached late. A refresh compares
    /// the buckets from the newest row less the rollup's lateness to the
    /// newest, and those the rollup has not folded yet; with `full`, every
    /// bucket. Counting a bucket's rows reads the time column's index, not
    /// the rows, so it takes well under a millisecond. Buckets older than
    /// the table's retention are not compared: the rollup keeps what it
    /// counted before the rows expired.
    ///
    /// A changed row that leaves a bucket's count as it was (an UPDATE, a
    /// delete and an insert into one bucket between two refreshes) is not
    /// seen until a fold of that bucket: rollups are for tables that rows
    /// are added to, such as logs.
    pub fn refresh_rollup(&self, name: &str, full: bool) -> Result<RefreshStats> {
        let r = self.rollup(name)?.ok_or_else(|| Error::Query(format!("no rollup {name}")))?;
        self.refresh(&r, full, &self.exec)
    }

    fn refresh(&self, r: &Rollup, full: bool, opts: &ExecOptions) -> Result<RefreshStats> {
        let t0 = Instant::now();
        let mut stats = RefreshStats::default();
        let base = self.table(&r.base)?;
        let roll = self.table(&r.name)?;
        let types = r.check(&base)?;
        let opts = all_rows(opts);
        let one = |sql: &str, p: &Params| -> Result<Option<Value>> { Ok(self.query_with(sql, p, &opts)?.rows.into_iter().next().and_then(|r| r.into_iter().next())) };
        let Some(newest) = time_of(one(&r.edge_sql(true), &Params::new())?.as_ref()) else { return Ok(stats) };
        let bucket = qsql::ident(&r.bucket);
        let folded = time_of(one(&r.folded_sql(), &Params::new())?.as_ref());
        let last = r.bucket_of(newest);
        let hot = r.bucket_of(newest.saturating_sub(r.lateness));
        let mut first = match (full, folded) {
            (false, Some(m)) => hot.min(m + r.width),
            _ => match time_of(one(&r.edge_sql(false), &Params::new())?.as_ref()) {
                Some(oldest) => r.bucket_of(oldest),
                None => return Ok(stats),
            },
        };
        // Buckets that rows have begun to leave by the table's retention
        // stay as they were folded.
        if let Some(keep) = &base.retain {
            let cutoff = crate::timestamp::now().saturating_sub(keep.seconds.saturating_mul(1_000_000));
            first = first.max(r.bucket_of(cutoff) + r.width);
        }
        if first > last {
            return Ok(stats);
        }
        let counts: HashMap<i64, i64> = {
            let sql = format!("SELECT {bucket}, SUM({}) FROM {} WHERE {bucket} >= :rollup_from AND {bucket} < :rollup_to GROUP BY {bucket}", qsql::ident(r.count_column()), qsql::ident(&r.name));
            self.query_with(&sql, &window(first, last + r.width), &opts)?.rows.into_iter().filter_map(|row| Some((time_of(row.first())?, int_of(row.get(1))))).collect()
        };
        // The table's count of each bucket, in one query of the time
        // column, a day of buckets at a time at most.
        let mut stale: Vec<i64> = Vec::new();
        let mut from = first;
        while from <= last {
            let to = (from + SPAN * r.width).min(last + r.width);
            let have: HashMap<i64, i64> = self.query_with(&r.bucket_counts_sql(), &window(from, to), &opts)?.rows.into_iter().filter_map(|row| Some((time_of(row.first())?, int_of(row.get(1))))).collect();
            let mut b = from;
            while b < to {
                stats.checked += 1;
                if have.get(&b).copied().unwrap_or(0) != counts.get(&b).copied().unwrap_or(0) {
                    stale.push(b);
                }
                b += r.width;
            }
            from = to;
        }
        // Folded in runs of nearby buckets, a query a run.
        let mut i = 0;
        while i < stale.len() {
            let mut j = i + 1;
            while j < stale.len() && stale[j] - stale[i] < SPAN * r.width && stale[j] - stale[j - 1] <= GAP * r.width {
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
            // In the groups' order, so that a fold of the same rows writes
            // the same keys.
            rows.sort_by_cached_key(|row| key_of(b, &row[..keys]));
            for (i, row) in rows.into_iter().enumerate() {
                let key = match text_keys {
                    true => Value::Str(key_of(b, &row[..keys])),
                    false => Value::Int(b.div_euclid(r.width) * GROUPS + i as i64),
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
                let full = tick % SWEEP_EVERY == SWEEP_EVERY - 1;
                tick += 1;
                let rollups = match db.rollups() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not read the rollups");
                        continue;
                    }
                };
                for r in rollups {
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| db.refresh(&r, full, &db.exec)));
                    match res {
                        Ok(Ok(s)) if s.folded > 0 => tracing::debug!(rollup = %r.name, checked = s.checked, folded = s.folded, rows = s.rows_written, secs = s.seconds, "refreshed a rollup"),
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(rollup = %r.name, error = %e, "a rollup's refresh failed; the next one tries again"),
                        Err(_) => tracing::warn!(rollup = %r.name, "a rollup's refresh panicked; the next one tries again"),
                    }
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the rollups' thread");
        }
    }
}
