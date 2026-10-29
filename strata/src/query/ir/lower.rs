//! Plans of bound statements: [`Plan::bind`], [`Plan::of_select`] and
//! [`Plan::of_write`].

use super::{Agg, Column, Expr, Graph, GraphKind, Node, OnConflict, Plan, Search, Set, SortKey};
use crate::query::logical::{self, GraphQuery, Having, OrderBy, Pred, Projection, Score, Seeds, Select, Tiebreak};
use crate::query::sql::{Assign, Conflict, Write};
use crate::schema::Schema;
use crate::value::Value;

impl Plan {
    /// Binds one SELECT of `schema`'s table ([`crate::query::bind`]) into
    /// its plan.
    pub fn bind(sql_text: &str, schema: &Schema, params: &crate::query::Params) -> crate::error::Result<Plan> {
        Ok(Plan::of_select(schema, &crate::query::bind(sql_text, schema, params)?))
    }

    /// The plan of a SELECT bound to `schema`'s table: `Project [Limit]
    /// Search`, or with aggregates `Project [Limit] [Sort] [Filter]
    /// Aggregate Search`.
    pub fn of_select(schema: &Schema, select: &Select) -> Plan {
        Plan::new(Lower { schema, graphs: &select.graphs }.select(select))
    }

    /// The plan of an INSERT, UPDATE or DELETE bound to `schema`'s table.
    pub fn of_write(schema: &Schema, write: &Write) -> Plan {
        let lower = Lower { schema, graphs: &[] };
        let table = schema.table.clone();
        Plan::new(match write {
            Write::Insert { docs, conflict } => Node::Insert {
                table,
                rows: docs.iter().map(|d| d.iter().enumerate().map(|(f, v)| lower.lit(f, v)).collect()).collect(),
                on_conflict: match conflict {
                    Conflict::Fail => OnConflict::Fail,
                    Conflict::Replace => OnConflict::Replace,
                    Conflict::Skip => OnConflict::Skip,
                    Conflict::Update(sets) => OnConflict::Update(sets.iter().map(|(f, a)| lower.set(*f, a)).collect()),
                },
            },
            Write::Update { select, sets } => Node::Update {
                table,
                input: Box::new(Lower { schema, graphs: &select.graphs }.select(select)),
                sets: sets.iter().map(|(f, a)| lower.set(*f, a)).collect(),
            },
            Write::Delete { select } => Node::Delete { table, input: Box::new(Lower { schema, graphs: &select.graphs }.select(select)) },
        })
    }
}

struct Lower<'a> {
    schema: &'a Schema,
    /// The graph computations of the Select being lowered.
    graphs: &'a [GraphQuery],
}

/// The columns an Aggregate gives: its keys, then the aggregates the
/// query names anywhere (in SELECT, HAVING or ORDER BY), each once.
struct Outputs {
    keys: Vec<usize>,
    aggs: Vec<Agg>,
}

impl Outputs {
    fn of(&mut self, p: &Projection) -> Expr {
        let agg = match p {
            Projection::Field(f) => return self.keys.iter().position(|k| k == f).map_or(Expr::Field(*f as u32), |i| Expr::Output(i as u32)),
            Projection::Score => return Expr::Score,
            Projection::Hops(_) => return Expr::Hops,
            Projection::Count => Agg { func: logical::AggFn::Count, arg: None },
            Projection::Agg { func, field } => Agg { func: *func, arg: Some(Expr::Field(*field as u32)) },
        };
        let i = self.aggs.iter().position(|a| *a == agg).unwrap_or_else(|| {
            self.aggs.push(agg);
            self.aggs.len() - 1
        });
        Expr::Output((self.keys.len() + i) as u32)
    }
}

/// A HAVING condition as an expression; `column` gives the expression of
/// each group value or aggregate it names.
pub(crate) fn having(h: &Having, column: &mut dyn FnMut(&Projection) -> Expr) -> Expr {
    match h {
        Having::Range { what, lo, hi } => Expr::Between {
            expr: Box::new(column(what)),
            lo: (*lo != f64::NEG_INFINITY).then(|| Box::new(Expr::Lit(Value::Float(*lo)))),
            hi: (*hi != f64::INFINITY).then(|| Box::new(Expr::Lit(Value::Float(*hi)))),
        },
        Having::Eq { what, value } => Expr::Cmp { op: super::CmpOp::Eq, left: Box::new(column(what)), right: Box::new(Expr::Lit(value.clone())) },
        Having::And(hs) => Expr::And(hs.iter().map(|h| having(h, column)).collect()),
        Having::Or(hs) => Expr::Or(hs.iter().map(|h| having(h, column)).collect()),
        Having::Not(h) => Expr::Not(Box::new(having(h, column))),
    }
}

fn aggregated(s: &Select) -> bool {
    !s.group_by.is_empty()
        || s.projection.iter().any(|(_, p)| matches!(p, Projection::Count | Projection::Agg { .. }))
        || matches!(s.order, Some(OrderBy::Count { .. } | OrderBy::GroupValue { .. } | OrderBy::Agg { .. }))
        || matches!(s.tiebreak, Some(Tiebreak::GroupValue { .. }))
}

impl Lower<'_> {
    fn select(&self, s: &Select) -> Node {
        let table = self.schema.table.clone();
        let filter = self.pred(&s.filter);
        let pk = || Expr::Field(self.schema.primary_key.unwrap_or(0) as u32);
        let node = if aggregated(s) {
            let mut out = Outputs { keys: s.group_by.clone(), aggs: Vec::new() };
            let columns: Vec<Column> = s.projection.iter().map(|(name, p)| Column { name: name.clone(), expr: out.of(p) }).collect();
            let having = s.having.as_ref().map(|h| self.having(h, &mut out));
            let mut keys = Vec::new();
            let key = |expr, desc| SortKey { expr, desc, nulls_first: false };
            match &s.order {
                None => {}
                Some(OrderBy::Count { desc }) => keys.push(key(out.of(&Projection::Count), *desc)),
                Some(OrderBy::GroupValue { desc }) => keys.push(key(Expr::Output(0), *desc)),
                Some(OrderBy::Agg { what, desc }) => keys.push(key(out.of(what), *desc)),
                Some(OrderBy::Score { score, desc, nulls_first }) => keys.push(SortKey { expr: self.score(score), desc: *desc, nulls_first: *nulls_first }),
            }
            match s.tiebreak {
                None => {}
                Some(Tiebreak::GroupValue { desc }) => keys.push(key(Expr::Output(0), desc)),
                Some(Tiebreak::Key { desc }) => keys.push(key(pk(), desc)),
            }
            let search = Node::Search(Search { table, filter, order: Vec::new() });
            let mut node = Node::Aggregate { input: Box::new(search), keys: out.keys.iter().map(|&f| Expr::Field(f as u32)).collect(), aggs: out.aggs };
            if let Some(cond) = having {
                node = Node::Filter { input: Box::new(node), cond };
            }
            if !keys.is_empty() {
                node = Node::Sort { input: Box::new(node), keys };
            }
            (node, columns)
        } else {
            let mut order = Vec::new();
            match &s.order {
                // Hops rank nearest first, as -hops, unless farthest first.
                Some(OrderBy::Score { score: Score::Hops { far_first, .. }, .. }) => order.push(SortKey { expr: Expr::Hops, desc: *far_first, nulls_first: false }),
                Some(OrderBy::Score { score, desc, nulls_first }) => order.push(SortKey { expr: self.score(score), desc: *desc, nulls_first: *nulls_first }),
                _ => {}
            }
            if let Some(Tiebreak::Key { desc }) = s.tiebreak {
                order.push(SortKey { expr: pk(), desc, nulls_first: false });
            }
            let columns = s
                .projection
                .iter()
                .map(|(name, p)| Column {
                    name: name.clone(),
                    expr: match p {
                        Projection::Field(f) => Expr::Field(*f as u32),
                        Projection::Hops(_) => Expr::Hops,
                        _ => Expr::Score,
                    },
                })
                .collect();
            (Node::Search(Search { table, filter, order }), columns)
        };
        let (mut node, columns) = node;
        if s.limit.is_some() || s.offset > 0 {
            node = Node::Limit { input: Box::new(node), offset: s.offset as u64, limit: s.limit.map(|l| l as u64) };
        }
        Node::Project { input: Box::new(node), columns }
    }

    /// A value of field `f` as a typed literal: a BOOL or TIMESTAMP
    /// field's stored integers as booleans and times.
    fn lit(&self, f: usize, v: &Value) -> Expr {
        Expr::Lit(self.schema.fields.get(f).map_or_else(|| v.clone(), |field| field.output(v.clone())))
    }

    fn pred(&self, p: &Pred) -> Expr {
        let field = |f: usize| Box::new(Expr::Field(f as u32));
        match p {
            Pred::True => Expr::Lit(Value::Bool(true)),
            Pred::False => Expr::Lit(Value::Bool(false)),
            Pred::Match { field, query, expand } => Expr::Match { field: *field as u32, query: query.clone(), expand: *expand },
            Pred::Range { field: f, lo, hi } => Expr::Between {
                expr: field(*f),
                lo: (*lo != f64::NEG_INFINITY).then(|| Box::new(Expr::Lit(Value::Float(*lo)))),
                hi: (*hi != f64::INFINITY).then(|| Box::new(Expr::Lit(Value::Float(*hi)))),
            },
            Pred::IntRange { field: f, lo, hi } => Expr::Between {
                expr: field(*f),
                lo: (*lo != i64::MIN).then(|| Box::new(self.lit(*f, &Value::Int(*lo)))),
                hi: (*hi != i64::MAX).then(|| Box::new(self.lit(*f, &Value::Int(*hi)))),
            },
            Pred::In { field: f, values } => Expr::In { expr: field(*f), list: values.iter().map(|v| self.lit(*f, v)).collect() },
            Pred::Present { field: f } => Expr::Not(Box::new(Expr::IsNull(field(*f)))),
            Pred::Not(q) => match &**q {
                Pred::Present { field: f } => Expr::IsNull(field(*f)),
                q => Expr::Not(Box::new(self.pred(q))),
            },
            Pred::And(ps) => Expr::And(ps.iter().map(|p| self.pred(p)).collect()),
            Pred::Or(ps) => Expr::Or(ps.iter().map(|p| self.pred(p)).collect()),
            Pred::Graph { id } => Expr::Graph(Box::new(self.graph(&self.graphs[*id]))),
        }
    }

    fn graph(&self, g: &GraphQuery) -> Graph {
        Graph {
            field: g.field as u32,
            seeds: self.seeds(&g.seeds),
            kind: match &g.kind {
                logical::GraphKind::Reach { min_hops, max_hops } => GraphKind::Reachable { min_hops: *min_hops, max_hops: *max_hops },
                logical::GraphKind::Path { to, max_hops } => GraphKind::Path { to: self.seeds(to), max_hops: *max_hops },
                logical::GraphKind::Ppr { alpha, eps, max_pushes, weighted } => GraphKind::Ppr { alpha: *alpha, eps: *eps, max_pushes: *max_pushes, weighted: *weighted },
            },
            dir: g.dir,
            labels: g.labels.clone(),
            through: g.through.as_ref().map(|p| self.pred(p)),
            max_nodes: g.max_nodes as u64,
            k: g.k as u64,
            min_sim: (g.min_sim != f64::NEG_INFINITY).then_some(g.min_sim),
        }
    }

    fn seeds(&self, s: &Seeds) -> Expr {
        match s {
            Seeds::Keys(keys) => Expr::Lit(Value::List(keys.clone())),
            Seeds::Query(q) => Expr::Subquery(Box::new(Lower { schema: self.schema, graphs: &q.graphs }.select(q))),
        }
    }

    fn score(&self, s: &Score) -> Expr {
        match s {
            Score::Bm25 { field, text, opts } => Expr::Bm25 { field: *field as u32, text: text.clone(), opts: opts.clone() },
            Score::Similarity { field, vector, opts } => Expr::Similarity { field: *field as u32, vector: vector.clone(), opts: opts.clone() },
            Score::Sparse { field, terms, opts } => Expr::Sparse { field: *field as u32, terms: terms.clone(), opts: opts.clone() },
            Score::Decay { field, decay, opts } => Expr::Decay { field: *field as u32, decay: *decay, opts: opts.clone() },
            Score::Rrf { inputs, k, depth } => Expr::Rrf { inputs: inputs.iter().map(|s| self.score(s)).collect(), k: *k, depth: *depth as u64 },
            Score::Weighted(terms) => Expr::Weighted(terms.iter().map(|(w, s)| (*w, self.score(s))).collect()),
            Score::Product(factors) => Expr::Product(factors.iter().map(|s| self.score(s)).collect()),
            Score::Column { field } => Expr::Field(*field as u32),
            Score::Ppr { graph } => Expr::Ppr(Box::new(self.graph(&self.graphs[*graph]))),
            Score::Hops { .. } => Expr::Hops,
        }
    }

    fn having(&self, h: &Having, out: &mut Outputs) -> Expr {
        having(h, &mut |p| out.of(p))
    }

    fn set(&self, f: usize, a: &Assign) -> Set {
        let value = match a {
            Assign::Value(v) => self.lit(f, v),
            Assign::Add(v) => Expr::Arith { op: super::ArithOp::Add, left: Box::new(Expr::Field(f as u32)), right: Box::new(Expr::Lit(v.clone())) },
            Assign::Excluded(g) => Expr::Excluded(*g as u32),
        };
        Set { field: f as u32, value }
    }
}
