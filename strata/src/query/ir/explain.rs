//! Plans as text, for `EXPLAIN`: a line per node, its input indented under
//! it, with fields by name and expressions in SQL's words.

use super::{Agg, ArithOp, CmpOp, Expr, Func, Graph, GraphKind, Node, OnConflict, Plan, SortKey};
use crate::query::logical::{AggFn, DecayShape, Direction, Expand, Fuzzy, Opts};
use crate::query::sql::ident;
use crate::schema::{FieldType, Logical, Schema};
use crate::value::Value;

/// Rows of an insert shown in full; the rest are counted.
const ROWS_SHOWN: usize = 5;

impl Plan {
    /// The plan as lines of text, the root first and each node's input
    /// indented under it. `schemas` name the fields of the tables the plan
    /// reads (a field of another table shows as `#id`).
    pub fn explain(&self, schemas: &[&Schema]) -> Vec<String> {
        Text { schemas }.node(&self.root).0
    }
}

struct Text<'a> {
    schemas: &'a [&'a Schema],
}

/// What the expressions over a node name: the fields of the table a
/// Search reads, or the columns an Aggregate gives.
#[derive(Clone, Default)]
struct Names<'a> {
    schema: Option<&'a Schema>,
    outputs: Vec<String>,
}

fn indent(lines: Vec<String>) -> impl Iterator<Item = String> {
    lines.into_iter().map(|l| format!("  {l}"))
}

impl<'a> Text<'a> {
    fn schema(&self, table: &str) -> Option<&'a Schema> {
        self.schemas.iter().copied().find(|s| s.table.eq_ignore_ascii_case(table))
    }

    fn node(&self, n: &Node) -> (Vec<String>, Names<'a>) {
        let over = |input: &Node, head: &dyn Fn(&Names<'a>) -> String| {
            let (lines, names) = self.node(input);
            (std::iter::once(head(&names)).chain(indent(lines)).collect::<Vec<_>>(), names)
        };
        match n {
            Node::Search(s) => {
                let names = Names { schema: self.schema(&s.table), outputs: Vec::new() };
                let mut lines = vec![format!("Search {}", ident(&s.table))];
                if !s.filter.is_always() {
                    lines.push(format!("  filter: {}", self.expr(&s.filter, &names)));
                }
                if !s.order.is_empty() {
                    lines.push(format!("  order: {}", self.keys(&s.order, &names)));
                }
                (lines, names)
            }
            Node::Aggregate { input, keys, aggs } => {
                let (lines, names) = self.node(input);
                let keys: Vec<String> = keys.iter().map(|k| self.expr(k, &names)).collect();
                let aggs: Vec<String> = aggs.iter().map(|a| self.agg(a, &names)).collect();
                let mut head = "Aggregate".to_string();
                if !keys.is_empty() {
                    head += &format!(" by {}", keys.join(", "));
                }
                if !aggs.is_empty() {
                    head += &format!(": {}", aggs.join(", "));
                }
                let names = Names { schema: names.schema, outputs: keys.into_iter().chain(aggs).collect() };
                (std::iter::once(head).chain(indent(lines)).collect(), names)
            }
            Node::Filter { input, cond } => over(input, &|names| format!("Filter: {}", self.expr(cond, names))),
            Node::Sort { input, keys } => over(input, &|names| format!("Sort: {}", self.keys(keys, names))),
            Node::Limit { input, offset, limit } => over(input, &|_| match (limit, offset) {
                (Some(l), 0) => format!("Limit: {l}"),
                (Some(l), o) => format!("Limit: {l} offset {o}"),
                (None, o) => format!("Offset: {o}"),
            }),
            Node::Project { input, columns } => over(input, &|names| {
                let cols: Vec<String> = columns
                    .iter()
                    .map(|c| {
                        let e = self.expr(&c.expr, names);
                        if e == c.name { e } else { format!("{e} AS {}", ident(&c.name)) }
                    })
                    .collect();
                format!("Project: {}", cols.join(", "))
            }),
            Node::Insert { table, rows, on_conflict } => {
                let names = Names { schema: self.schema(table), outputs: Vec::new() };
                let mut head = format!("Insert into {}: {} row{}", ident(table), rows.len(), if rows.len() == 1 { "" } else { "s" });
                match on_conflict {
                    OnConflict::Fail => {}
                    OnConflict::Replace => head += ", replacing a live key",
                    OnConflict::Skip => head += ", skipping a live key",
                    OnConflict::Update(sets) => head += &format!(", on a live key SET {}", self.sets(sets, &names)),
                }
                let mut lines = vec![head];
                if let Some(s) = names.schema {
                    lines.push(format!("  columns: {}", s.fields.iter().map(|f| ident(&f.name)).collect::<Vec<_>>().join(", ")));
                }
                for row in rows.iter().take(ROWS_SHOWN) {
                    lines.push(format!("  ({})", row.iter().map(|e| self.expr(e, &names)).collect::<Vec<_>>().join(", ")));
                }
                if rows.len() > ROWS_SHOWN {
                    lines.push(format!("  … {} more", rows.len() - ROWS_SHOWN));
                }
                (lines, names)
            }
            Node::Update { table, input, sets } => {
                let names = Names { schema: self.schema(table), outputs: Vec::new() };
                let (lines, _) = self.node(input);
                (std::iter::once(format!("Update {} SET {}", ident(table), self.sets(sets, &names))).chain(indent(lines)).collect(), names)
            }
            Node::Delete { table, input } => {
                let (lines, names) = self.node(input);
                (std::iter::once(format!("Delete from {}", ident(table))).chain(indent(lines)).collect(), names)
            }
        }
    }

    fn keys(&self, keys: &[SortKey], names: &Names) -> String {
        keys.iter()
            .map(|k| format!("{}{}{}", self.expr(&k.expr, names), if k.desc { " DESC" } else { "" }, if k.nulls_first { " NULLS FIRST" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn sets(&self, sets: &[super::Set], names: &Names) -> String {
        sets.iter().map(|s| format!("{} = {}", field(names, s.field), self.expr(&s.value, names))).collect::<Vec<_>>().join(", ")
    }

    fn agg(&self, a: &Agg, names: &Names) -> String {
        let arg = a.arg.as_ref().map_or("*".to_string(), |e| self.expr(e, names));
        match a.func {
            AggFn::Count => format!("COUNT({arg})"),
            AggFn::CountDistinct => format!("COUNT(DISTINCT {arg})"),
            AggFn::Sum => format!("SUM({arg})"),
            AggFn::Avg => format!("AVG({arg})"),
            AggFn::Min => format!("MIN({arg})"),
            AggFn::Max => format!("MAX({arg})"),
        }
    }

    /// `e`, in parentheses if it is AND, OR or an operator's.
    fn operand(&self, e: &Expr, names: &Names) -> String {
        match e {
            Expr::And(_) | Expr::Or(_) | Expr::Weighted(_) | Expr::Arith { .. } | Expr::Concat(_) | Expr::Like { .. } => format!("({})", self.expr(e, names)),
            e => self.expr(e, names),
        }
    }

    fn expr(&self, e: &Expr, names: &Names) -> String {
        match e {
            Expr::Field(f) => field(names, *f),
            Expr::Score => "score()".into(),
            Expr::Hops => "hops()".into(),
            Expr::Output(i) => names.outputs.get(*i as usize).cloned().unwrap_or_else(|| format!("#{i}")),
            Expr::Excluded(f) => format!("excluded.{}", field(names, *f)),
            Expr::Lit(v) => lit(v),
            Expr::Not(x) => match &**x {
                Expr::IsNull(y) => format!("{} IS NOT NULL", self.operand(y, names)),
                x @ (Expr::Field(_) | Expr::Lit(_) | Expr::Match { .. } | Expr::Graph(_)) => format!("NOT {}", self.expr(x, names)),
                x => format!("NOT ({})", self.expr(x, names)),
            },
            Expr::And(xs) => xs.iter().map(|x| self.operand(x, names)).collect::<Vec<_>>().join(" AND "),
            Expr::Or(xs) => xs.iter().map(|x| self.operand(x, names)).collect::<Vec<_>>().join(" OR "),
            Expr::IsNull(x) => format!("{} IS NULL", self.operand(x, names)),
            Expr::Cmp { op, left, right } => {
                let op = match op {
                    CmpOp::Eq => "=",
                    CmpOp::Ne => "!=",
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                };
                format!("{} {op} {}", self.operand(left, names), self.operand(right, names))
            }
            Expr::Between { expr, lo, hi } => {
                let x = self.operand(expr, names);
                match (lo, hi) {
                    (Some(lo), Some(hi)) if lo == hi => format!("{x} = {}", self.expr(lo, names)),
                    (Some(lo), Some(hi)) => format!("{x} BETWEEN {} AND {}", self.expr(lo, names), self.expr(hi, names)),
                    (Some(lo), None) => format!("{x} >= {}", self.expr(lo, names)),
                    (None, Some(hi)) => format!("{x} <= {}", self.expr(hi, names)),
                    (None, None) => format!("{x} BETWEEN -inf AND inf"),
                }
            }
            Expr::In { expr, list } => match &list[..] {
                [v] => format!("{} = {}", self.operand(expr, names), self.expr(v, names)),
                vs => format!("{} IN ({})", self.operand(expr, names), vs.iter().map(|v| self.expr(v, names)).collect::<Vec<_>>().join(", ")),
            },
            Expr::Arith { op, left, right } => {
                let op = match op {
                    ArithOp::Add => "+",
                    ArithOp::Sub => "-",
                    ArithOp::Mul => "*",
                    ArithOp::Div => "/",
                    ArithOp::Mod => "%",
                };
                format!("{} {op} {}", self.operand(left, names), self.operand(right, names))
            }
            Expr::Neg(x) => format!("-{}", self.operand(x, names)),
            Expr::Concat(xs) => xs.iter().map(|x| self.operand(x, names)).collect::<Vec<_>>().join(" || "),
            Expr::Like { expr, pattern, case_insensitive } => {
                format!("{} {} {}", self.operand(expr, names), if *case_insensitive { "ILIKE" } else { "LIKE" }, self.operand(pattern, names))
            }
            Expr::Case { operand, whens, otherwise } => {
                let mut s = "CASE".to_string();
                if let Some(o) = operand {
                    s += &format!(" {}", self.operand(o, names));
                }
                for w in whens {
                    s += &format!(" WHEN {} THEN {}", self.expr(&w.when, names), self.expr(&w.then, names));
                }
                if let Some(x) = otherwise {
                    s += &format!(" ELSE {}", self.expr(x, names));
                }
                s + " END"
            }
            Expr::Call { func, args } => {
                let name = match func {
                    Func::Lower => "lower",
                    Func::Upper => "upper",
                    Func::Length => "length",
                    Func::Abs => "abs",
                    Func::Round => "round",
                    Func::Coalesce => "coalesce",
                };
                format!("{name}({})", args.iter().map(|x| self.expr(x, names)).collect::<Vec<_>>().join(", "))
            }
            Expr::Match { field: f, query, expand } => format!("MATCH({}, {}{})", field(names, *f), quote(query), expansion(expand)),
            Expr::Graph(g) | Expr::Ppr(g) => self.graph(g, names),
            Expr::Bm25 { field: f, text, opts } => format!("bm25({}, {}{})", field(names, *f), quote(text), options(opts)),
            Expr::Similarity { field: f, vector, opts } => format!("similarity({}, {}{})", field(names, *f), lit(&Value::Vector(vector.clone())), options(opts)),
            Expr::Sparse { field: f, terms, opts } => {
                let terms: serde_json::Map<String, serde_json::Value> = terms.iter().map(|(t, w)| (t.clone(), serde_json::json!(w))).collect();
                format!("similarity({}, {}{})", field(names, *f), quote(&serde_json::Value::Object(terms).to_string()), options(opts))
            }
            Expr::Decay { field: f, decay, .. } => {
                let shape = match decay.shape {
                    DecayShape::Exp => "exp",
                    DecayShape::Gauss => "gauss",
                    DecayShape::Linear => "linear",
                };
                // A TIMESTAMP's origin is a time, and its scale and offset
                // seconds, as SQL gives them.
                let time = names.schema.and_then(|s| s.fields.get(*f as usize)).is_some_and(|fl| fl.logical == Some(Logical::Timestamp));
                let (origin, unit) = match time {
                    true => (lit(&Value::Timestamp(decay.origin as i64)), 1e6),
                    false => (decay.origin.to_string(), 1.0),
                };
                format!("decay({}, {origin}, {}, offset => {}, decay => {}, shape => '{shape}')", field(names, *f), decay.scale / unit, decay.offset / unit, decay.decay)
            }
            Expr::Rrf { inputs, k, depth } => format!("rrf({}, k => {k}, depth => {depth})", inputs.iter().map(|x| self.expr(x, names)).collect::<Vec<_>>().join(", ")),
            Expr::Weighted(terms) => {
                let mut s = String::new();
                for (i, (w, x)) in terms.iter().enumerate() {
                    // A negative weight after the first term is a minus.
                    let w = match i {
                        0 => *w,
                        _ if *w < 0.0 => {
                            s += " - ";
                            -w
                        }
                        _ => {
                            s += " + ";
                            *w
                        }
                    };
                    s += &match w {
                        w if w == 1.0 => self.operand(x, names),
                        w => format!("{w} * {}", self.operand(x, names)),
                    };
                }
                s
            }
            Expr::Product(xs) => xs.iter().map(|x| self.operand(x, names)).collect::<Vec<_>>().join(" * "),
            Expr::Subquery(n) => {
                let (lines, _) = self.node(n);
                format!("({})", lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join("; "))
            }
        }
    }

    fn graph(&self, g: &Graph, names: &Names) -> String {
        let f = field(names, g.field);
        let seeds = self.seeds(&g.seeds, names);
        let dir = match g.dir {
            Direction::Out => "out",
            Direction::In => "in",
            Direction::Both => "both",
        };
        let mut s = match &g.kind {
            GraphKind::Reachable { min_hops, max_hops } => format!("REACHABLE({f}, {seeds}, hops => {max_hops}, min_hops => {min_hops}, dir => '{dir}', max_nodes => {}", g.max_nodes),
            GraphKind::Path { to, max_hops } => format!("PATH({f}, {seeds}, {}, max_hops => {max_hops}, dir => '{dir}', max_nodes => {}", self.seeds(to, names), g.max_nodes),
            GraphKind::Ppr { alpha, eps, max_pushes, weighted } => {
                format!("ppr({f}, {seeds}, alpha => {alpha}, eps => {eps}, max_pushes => {max_pushes}, weighted => {}, dir => '{dir}'", *weighted as u8)
            }
        };
        if !g.labels.is_empty() {
            s += &format!(", labels => {}", quote(&g.labels.join(",")));
        }
        if let Some(t) = &g.through {
            s += &format!(", through => {}", self.expr(t, names));
        }
        let vector = names.schema.and_then(|sc| sc.fields.get(g.field as usize)).is_some_and(|fl| matches!(fl.ty, FieldType::Vector { .. }));
        if vector {
            s += &format!(", k => {}", g.k);
        }
        if let Some(m) = g.min_sim {
            s += &format!(", min_sim => {m}");
        }
        s + ")"
    }

    fn seeds(&self, e: &Expr, names: &Names) -> String {
        match e {
            Expr::Lit(Value::List(keys)) if keys.len() == 1 => lit(&keys[0]),
            Expr::Lit(Value::List(keys)) => format!("({})", keys.iter().map(lit).collect::<Vec<_>>().join(", ")),
            e => self.expr(e, names),
        }
    }
}

fn field(names: &Names, f: u32) -> String {
    names.schema.and_then(|s| s.fields.get(f as usize)).map_or_else(|| format!("#{f}"), |fl| ident(&fl.name))
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A value as SQL writes it; a long vector by its first components.
fn lit(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(x) => x.to_string(),
        Value::Float(x) => format!("{x:?}"),
        Value::Str(s) => quote(s),
        Value::Timestamp(t) => format!("TIMESTAMP {}", quote(&crate::timestamp::format(*t))),
        Value::Vector(xs) => {
            let shown: Vec<String> = xs.iter().take(4).map(|x| format!("{x:?}")).collect();
            if xs.len() > 4 {
                format!("[{}, … ({} values)]", shown.join(", "), xs.len())
            } else {
                format!("[{}]", shown.join(", "))
            }
        }
        Value::List(xs) => format!("ARRAY[{}]", xs.iter().map(lit).collect::<Vec<_>>().join(", ")),
    }
}

fn expansion(e: &Expand) -> String {
    let mut s = String::new();
    if e.synonyms {
        s += ", synonyms => 1";
    }
    match e.fuzzy {
        None => {}
        Some(Fuzzy::Auto) => s += ", fuzzy => 'auto'",
        Some(Fuzzy::Edits(n)) => s += &format!(", fuzzy => {n}"),
    }
    s
}

fn options(opts: &Opts) -> String {
    opts.iter().map(|(k, v)| format!(", {k} => {}", lit(v))).collect()
}
