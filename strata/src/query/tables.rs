//! The tables a query names, so that a database of several tables can run it
//! on the one it reads ([`crate::db::Db::query`]).

use std::collections::HashSet;

use sqlparser::ast::{self as sql, Expr, SelectItem, TableFactor};

use crate::query::compound::walk;

/// The tables `q` reads, each once (in any case), in the order it first
/// names them: every `FROM` and `JOIN`, in `WITH` queries, derived tables
/// and subqueries too, but not the `WITH` results it names. As in
/// [`crate::query::compound`], a `WITH` query sees the results defined
/// before it, so `WITH papers AS (SELECT ... FROM papers)` reads the table.
pub fn read_by(q: &sql::Query) -> Vec<String> {
    let mut w = Names::default();
    w.query(q);
    let mut seen = HashSet::new();
    w.tables.into_iter().filter(|t| seen.insert(t.to_ascii_lowercase())).collect()
}

#[derive(Default)]
struct Names {
    tables: Vec<String>,
    /// Names of the `WITH` results defined so far, lowercased.
    ctes: HashSet<String>,
}

impl Names {
    fn query(&mut self, q: &sql::Query) {
        if let Some(w) = &q.with {
            for cte in &w.cte_tables {
                self.query(&cte.query);
                self.ctes.insert(cte.alias.name.value.to_ascii_lowercase());
            }
        }
        self.body(&q.body);
        if let Some(ob) = &q.order_by {
            if let sql::OrderByKind::Expressions(es) = &ob.kind {
                for e in es {
                    self.expr(&e.expr);
                }
            }
        }
    }

    fn body(&mut self, b: &sql::SetExpr) {
        match b {
            sql::SetExpr::Select(s) => {
                for t in &s.from {
                    self.from(t);
                }
                for item in &s.projection {
                    if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
                        self.expr(e);
                    }
                }
                for e in s.selection.iter().chain(s.having.iter()) {
                    self.expr(e);
                }
                if let sql::GroupByExpr::Expressions(es, _) = &s.group_by {
                    es.iter().for_each(|e| self.expr(e));
                }
            }
            sql::SetExpr::Query(q) => self.query(q),
            sql::SetExpr::SetOperation { left, right, .. } => {
                self.body(left);
                self.body(right);
            }
            _ => {}
        }
    }

    fn from(&mut self, t: &sql::TableWithJoins) {
        self.factor(&t.relation);
        for j in &t.joins {
            self.factor(&j.relation);
            let on = match &j.join_operator {
                sql::JoinOperator::Join(c) | sql::JoinOperator::Inner(c) | sql::JoinOperator::Left(c) | sql::JoinOperator::LeftOuter(c) => Some(c),
                _ => None,
            };
            if let Some(sql::JoinConstraint::On(e)) = on {
                self.expr(e);
            }
        }
    }

    fn factor(&mut self, f: &TableFactor) {
        match f {
            TableFactor::Table { name, .. } => {
                let name = name.to_string();
                if !self.ctes.contains(&name.to_ascii_lowercase()) {
                    self.tables.push(name);
                }
            }
            TableFactor::Derived { subquery, .. } => self.query(subquery),
            TableFactor::NestedJoin { table_with_joins, .. } => self.from(table_with_joins),
            _ => {}
        }
    }

    /// The subqueries of `e`, at any depth.
    fn expr(&mut self, e: &Expr) {
        walk(e, &mut |x| match x {
            Expr::InSubquery { subquery, .. } | Expr::Exists { subquery, .. } | Expr::Subquery(subquery) => self.query(subquery),
            _ => {}
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Vec<String> {
        let stmts = crate::query::parse::parse(text).unwrap();
        let sql::Statement::Query(q) = &stmts[0] else { panic!() };
        read_by(q)
    }

    #[test]
    fn finds_every_table_a_query_reads() {
        assert_eq!(read("SELECT id FROM papers WHERE year > 2000"), ["papers"]);
        assert_eq!(read("SELECT a.id FROM papers a JOIN Papers b ON a.cites = b.id"), ["papers"]);
        assert_eq!(read("SELECT id FROM papers WHERE author IN (SELECT id FROM authors WHERE name = 'x')"), ["papers", "authors"]);
        assert_eq!(read("WITH top AS (SELECT id FROM authors LIMIT 5) SELECT COUNT(*) FROM top"), ["authors"]);
        assert_eq!(read("WITH authors AS (SELECT id FROM authors LIMIT 5) SELECT COUNT(*) FROM authors"), ["authors"]);
        assert_eq!(read("WITH a AS (SELECT id FROM t1), b AS (SELECT id FROM a) SELECT id FROM b JOIN t2 ON b.id = t2.id"), ["t1", "t2"]);
        assert_eq!(read("SELECT n FROM (SELECT COUNT(*) AS n FROM venues) AS t"), ["venues"]);
        assert_eq!(read("SELECT id FROM papers WHERE REACHABLE(cites, (SELECT id FROM papers LIMIT 2)) AND EXISTS (SELECT id FROM tags)"), ["papers", "tags"]);
        assert_eq!(read("SELECT id FROM papers ORDER BY year > (SELECT AVG(year) FROM other) DESC"), ["papers", "other"]);
        assert!(read("SELECT 1").is_empty());
    }
}
