//! Statement budgets: what one statement may cost, checked on the
//! statement as it plans, after its stored functions and rank profiles
//! are written out, and on what it reads as it runs.
//!
//! The text of a statement is bounded before it is parsed (its depth,
//! [`super::parse::MAX_EXPR_DEPTH`]; a server's `--max-statement-bytes`
//! and `--max-statement-elements`), and its calls written out are held to
//! [`super::function::MAX_WRITTEN_OUT`] expressions. Those are the most
//! any statement may be. A budget is less, chosen by whoever runs it:
//!
//! | budget | bounds | checked |
//! |---|---|---|
//! | `nodes` | the expressions of the statement written out | after expansion, before it binds |
//! | `depth` | how deep they nest (a subquery is a level) | after expansion, before it binds |
//! | `list` | the values of one list: `IN (...)`, `ARRAY[...]`, a list parameter | after expansion, before it binds |
//! | `plan_ms` | the time one plan takes to make | while it is made, as the deadline is |
//! | `rows_read` | the rows the statement's scans and groups visit, and the rows its parts answer | as it runs, every 1,024 rows, and when each part answers |
//!
//! A budget is the tightest of the ones that apply ([`Budget::within`]):
//! embedded, [`super::exec::ExecOptions::budget`]; on a server, its
//! `--statement-budget`, the key's budget and the request's `budget`, so a
//! request may lower its key's budget but never raise it. A budget left
//! out is no bound. Every entry point that plans a statement plans it
//! through [`guarded`]: on the stack a deep statement needs, stopped at
//! its deadline, within its budget.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlparser::ast::{Expr, Query, Visit, Visitor};

use super::exec::ExecOptions;
use super::sql::Params;
use crate::error::{Error, Result};
use crate::value::Value;

/// What one statement may cost ([module docs](self)); `None` is no bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_read: Option<u64>,
}

/// The budgets' names, as a spec ([`Budget::parse`]) and JSON spell them.
pub const NAMES: [&str; 5] = ["nodes", "depth", "list", "plan_ms", "rows_read"];

fn tighter(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl Budget {
    /// No bound at all.
    pub fn is_none(&self) -> bool {
        *self == Budget::default()
    }

    /// The tighter of `self` and `other`, each bound by itself.
    pub fn within(self, other: Budget) -> Budget {
        Budget {
            nodes: tighter(self.nodes, other.nodes),
            depth: tighter(self.depth, other.depth),
            list: tighter(self.list, other.list),
            plan_ms: tighter(self.plan_ms, other.plan_ms),
            rows_read: tighter(self.rows_read, other.rows_read),
        }
    }

    /// Whether the statement's shape is bounded (`nodes`, `depth` or
    /// `list`).
    fn bounds_shape(&self) -> bool {
        self.nodes.is_some() || self.depth.is_some() || self.list.is_some()
    }

    fn slot(&mut self, name: &str) -> Option<&mut Option<u64>> {
        Some(match name {
            "nodes" => &mut self.nodes,
            "depth" => &mut self.depth,
            "list" => &mut self.list,
            "plan_ms" => &mut self.plan_ms,
            "rows_read" => &mut self.rows_read,
            _ => return None,
        })
    }

    /// A budget from its spec, `name=n` pairs apart by commas
    /// (`nodes=20000,rows_read=1000000`); an empty spec is no bound, and
    /// `name=0` or `name=none` leaves that one out.
    pub fn parse(spec: &str) -> Result<Budget> {
        let mut b = Budget::default();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (name, value) = part.split_once('=').ok_or_else(|| Error::Query(format!("a statement budget is name=value pairs apart by commas ({}), not {part}", NAMES.join(", "))))?;
            let name = name.trim().to_ascii_lowercase();
            let slot = b.slot(&name).ok_or_else(|| Error::Query(format!("no statement budget is named {name}: they are {}", NAMES.join(", "))))?;
            let value = value.trim().replace('_', "");
            *slot = match value.as_str() {
                "none" | "0" => None,
                v => Some(v.parse::<u64>().map_err(|_| Error::Query(format!("statement budget {name} is a whole number, not {v}")))?),
            };
        }
        Ok(b)
    }

    /// A budget from JSON: an object of the names to whole numbers
    /// (`{"nodes": 20000}`), or a spec ([`Budget::parse`]); `null` is no
    /// bound.
    pub fn from_json(v: &serde_json::Value) -> Result<Budget> {
        match v {
            serde_json::Value::Null => Ok(Budget::default()),
            serde_json::Value::String(s) => Budget::parse(s),
            v => {
                let b: Budget = serde_json::from_value(v.clone()).map_err(|e| Error::Query(format!("a statement budget is an object of {} to whole numbers: {e}", NAMES.join(", "))))?;
                Ok(b.without_zeros())
            }
        }
    }

    /// 0 read as no bound, as in a spec.
    fn without_zeros(mut self) -> Budget {
        for name in NAMES {
            let slot = self.slot(name).expect("a budget's name");
            if *slot == Some(0) {
                *slot = None;
            }
        }
        self
    }

    /// The spec of this budget ([`Budget::parse`] reads it back).
    pub fn spec(&self) -> String {
        let mut b = *self;
        NAMES.iter().filter_map(|n| b.slot(n).expect("a budget's name").map(|v| format!("{n}={v}"))).collect::<Vec<_>>().join(",")
    }
}

/// What a statement's shape costs ([`shape_of`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    /// Its expressions.
    pub nodes: u64,
    /// How deep its expressions and queries nest.
    pub depth: u64,
    /// The values of its longest list.
    pub list: u64,
}

#[derive(Default)]
struct Measure {
    shape: Shape,
    at: u64,
}

impl Measure {
    fn down(&mut self) {
        self.at += 1;
        self.shape.depth = self.shape.depth.max(self.at);
    }
}

impl Visitor for Measure {
    type Break = ();

    fn pre_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
        self.down();
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
        self.at -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        self.shape.nodes += 1;
        self.down();
        let n = match e {
            Expr::InList { list, .. } => list.len(),
            Expr::Array(a) => a.elem.len(),
            Expr::Tuple(t) => t.len(),
            _ => 0,
        };
        self.shape.list = self.shape.list.max(n as u64);
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, _: &Expr) -> ControlFlow<()> {
        self.at -= 1;
        ControlFlow::Continue(())
    }
}

/// The shape of `v` (a query or a statement): its expressions, how deep
/// they nest, and its longest list.
pub fn shape_of<V: Visit>(v: &V) -> Shape {
    let mut m = Measure::default();
    let _ = v.visit(&mut m);
    m.shape
}

/// The values of the longest list among `params`.
pub fn longest_list(params: &Params) -> u64 {
    params.0.values().map(|v| if let Value::List(l) = v { l.len() as u64 } else { 0 }).max().unwrap_or(0)
}

const ADVICE: &str = "a statement's budget is the tightest of the server's --statement-budget, its key's and the request's \"budget\" (embedded, ExecOptions::budget)";

/// A query error when `v`, a statement written out (its stored functions
/// and rank profiles in place), or one of `params`' lists, is past
/// `budget`'s `nodes`, `depth` or `list`.
pub fn check_shape<V: Visit>(budget: &Budget, v: &V, params: &Params) -> Result<()> {
    if !budget.bounds_shape() {
        return Ok(());
    }
    if let Some(max) = budget.list {
        let n = longest_list(params);
        if n > max {
            return Err(Error::Query(format!("a list parameter holds {n} values, more than the statement's budget of {max} (list; {ADVICE}): split the list over several statements")));
        }
    }
    let s = shape_of(v);
    if let Some(max) = budget.nodes.filter(|&m| s.nodes > m) {
        return Err(Error::Query(format!(
            "the statement holds {} expressions written out (its stored functions and rank profiles in place), more than its budget of {max} (nodes; {ADVICE}): write it, or the functions it calls, with fewer terms",
            s.nodes
        )));
    }
    if let Some(max) = budget.depth.filter(|&m| s.depth > m) {
        return Err(Error::Query(format!(
            "the statement nests {} levels deep written out (its stored functions and rank profiles in place), more than its budget of {max} (depth; {ADVICE}): nest fewer subqueries, calls or brackets",
            s.depth
        )));
    }
    if let Some(max) = budget.list.filter(|&m| s.list > m) {
        return Err(Error::Query(format!(
            "a list in the statement holds {} values, more than its budget of {max} (list; {ADVICE}): split the list over several statements",
            s.list
        )));
    }
    Ok(())
}

/// A query error when statement text `sql` as written, or one of
/// `params`' lists, is past `budget`'s shape ([`check_shape`]); nothing is
/// parsed when the budget bounds no shape. A query is held to it again
/// once its stored functions and rank profiles are written out, as it
/// plans.
pub fn check_text(budget: &Budget, sql: &str, params: &Params) -> Result<()> {
    if !budget.bounds_shape() {
        return Ok(());
    }
    for st in super::parse::parse(sql)? {
        check_shape(budget, &st, params)?;
    }
    Ok(())
}

/// The rows a statement read, counted against its `rows_read` budget
/// ([`ExecOptions::reads`]); every part of the statement (its leaves,
/// steps and subqueries) counts into the same one.
#[derive(Debug)]
pub struct Reads {
    limit: u64,
    n: AtomicU64,
}

impl Reads {
    pub fn new(limit: u64) -> Arc<Reads> {
        Arc::new(Reads { limit, n: AtomicU64::new(0) })
    }

    /// The rows read so far.
    pub fn read(&self) -> u64 {
        self.n.load(Ordering::Relaxed)
    }

    /// `n` more rows read, without a check (what a loop read since its
    /// last check, as it ends).
    pub fn add(&self, n: u64) {
        self.n.fetch_add(n, Ordering::Relaxed);
    }

    /// `n` more rows read: a query error once the statement read more
    /// than its budget.
    pub fn charge(&self, n: u64) -> Result<()> {
        let total = self.n.fetch_add(n, Ordering::Relaxed) + n;
        if total > self.limit {
            return Err(Error::Query(format!(
                "the statement read more than {} rows, its budget (rows_read; {ADVICE}): narrow it with a filter the indexes answer, or a LIMIT",
                self.limit
            )));
        }
        Ok(())
    }
}

/// `opts` with a count of the rows read against its budget's
/// `rows_read`, when it has one and no count yet (a step of a statement
/// counts into its statement's).
pub fn counted(opts: &ExecOptions) -> std::borrow::Cow<'_, ExecOptions> {
    match opts.budget.rows_read {
        Some(limit) if opts.reads.is_none() => std::borrow::Cow::Owned(ExecOptions { reads: Some(Reads::new(limit)), ..opts.clone() }),
        _ => std::borrow::Cow::Borrowed(opts),
    }
}

/// `f`, the planning (and run) of statement `sql`, as every entry point
/// plans one: on a thread with the stack for it when it nests deep
/// ([`super::parse::deep_stack`]), stopped at `opts`' deadline and cancel
/// flag and its budget's `plan_ms` ([`super::compound::planning_under`]),
/// its shape held to the budget once written out
/// ([`super::compound`] checks it as it binds), and what it reads counted
/// ([`counted`]). `f` is given the options to run with.
pub fn guarded<T: Send>(sql: &str, opts: &ExecOptions, f: impl FnOnce(&ExecOptions) -> Result<T> + Send) -> Result<T> {
    let opts = counted(opts);
    let opts = &*opts;
    super::parse::deep_stack(super::parse::nests_deep(sql), || super::compound::planning_under(opts, || f(opts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_parse_combine_and_print() {
        let b = Budget::parse("nodes=1_000, rows_read = 50000,depth=none").unwrap();
        assert_eq!(b, Budget { nodes: Some(1000), rows_read: Some(50_000), ..Budget::default() });
        assert_eq!(Budget::parse(&b.spec()).unwrap(), b);
        assert!(Budget::parse("").unwrap().is_none());
        assert!(Budget::parse("rows=5").unwrap_err().to_string().contains("rows_read"));
        assert!(Budget::parse("nodes=many").unwrap_err().to_string().contains("whole number"));
        let k = Budget { nodes: Some(500), list: Some(10), ..Budget::default() };
        assert_eq!(b.within(k), Budget { nodes: Some(500), list: Some(10), rows_read: Some(50_000), ..Budget::default() });
        assert_eq!(Budget::from_json(&serde_json::json!({"nodes": 7, "list": 0})).unwrap(), Budget { nodes: Some(7), ..Budget::default() });
        assert_eq!(Budget::from_json(&serde_json::json!("plan_ms=20")).unwrap().plan_ms, Some(20));
        assert!(Budget::from_json(&serde_json::json!({"node": 7})).is_err());
    }

    #[test]
    fn shapes_count_expressions_depth_and_lists() {
        let q = super::super::parse::parse("SELECT a FROM t WHERE b IN (1, 2, 3) AND c = (SELECT MAX(d) FROM u WHERE e = 1)").unwrap();
        let s = shape_of(&q[0]);
        assert_eq!(s.list, 3);
        assert!(s.nodes >= 10, "{s:?}");
        let flat = shape_of(&super::super::parse::parse("SELECT a FROM t").unwrap()[0]);
        assert!(s.depth > flat.depth, "{s:?} {flat:?}");
        let b = Budget { list: Some(2), ..Budget::default() };
        assert!(check_shape(&b, &q[0], &Params::new()).unwrap_err().to_string().contains("holds 3 values"));
        let p = Params::new().set("ids", Value::List((0..5).map(Value::Int).collect()));
        let b = Budget { list: Some(4), ..Budget::default() };
        assert!(check_shape(&b, &flat_query(), &p).unwrap_err().to_string().contains("list parameter holds 5"));
        assert!(check_shape(&Budget { nodes: Some(3), ..Budget::default() }, &q[0], &Params::new()).unwrap_err().to_string().contains("(nodes;"));
        assert!(check_shape(&Budget { depth: Some(2), ..Budget::default() }, &q[0], &Params::new()).unwrap_err().to_string().contains("(depth;"));
    }

    fn flat_query() -> sqlparser::ast::Statement {
        super::super::parse::parse("SELECT a FROM t WHERE b IN (:ids)").unwrap().remove(0)
    }

    #[test]
    fn reads_stop_past_the_budget() {
        let r = Reads::new(10);
        r.add(4);
        assert!(r.charge(6).is_ok());
        assert!(r.charge(1).unwrap_err().to_string().contains("more than 10 rows"));
        assert_eq!(r.read(), 11);
    }
}
