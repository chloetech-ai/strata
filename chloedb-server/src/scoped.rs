//! What a key limited to tables may do ([`crate::keys::Grant`]): which
//! routes it may call at all, and which tables a statement it sends
//! touches.
//!
//! Such a key is for an app beside the cluster (the crawler, the
//! spreadsheets, ChloeCtx, the app engine): it reads and writes its own
//! tables, perhaps reads others, and does nothing else. So its routes are
//! a list ([`route_ok`]), every other route is refused whatever its grants,
//! and each route on the list checks the tables it touches:
//!
//! - a statement ([`check_statement`]) by its kind (a query reads, an
//!   `INSERT`, `UPDATE` or `DELETE` writes its table, `CREATE`, `ALTER` and
//!   `DROP` need admin on theirs) and by every table it names, read
//!   wherever it is named: `FROM` and `JOIN`, subqueries, `WITH`, the
//!   source of an `INSERT ... SELECT`, an edge table a walk names;
//! - a write, a read by key, an import, a table's schema: by the table
//!   they name.
//!
//! The statement is parsed by ChloeDB's own parser
//! ([`chloedb::query::parse::parse`]), so that it is read here as the
//! engine reads it. What cannot be checked by its names is refused rather
//! than allowed: a table function that is not a stored one (a stored
//! one is checked by the tables its body reads, written out as the engine
//! runs it), a `WITH` table named as a table is, a name in parts (`a.b`),
//! a call of `llm()` (its second step is SQL a model writes) or of
//! `encode()` (a sequence model reads its own table's vectors), and a
//! stored function when one of them calls `llm()`.
//!
//! Such a key with the admin scope on some tables may define a stored
//! function no one has, or the same one again, whose body reads only
//! tables it may read ([`check_function`]): an app installs its own SQL
//! functions. It replaces and drops none.

use std::collections::BTreeSet;
use std::ops::ControlFlow;

use axum::http::Method;
use chloedb::query::function::FunctionSet;
use chloedb::query::sql::Command;
use sqlparser::ast::{self as sql, Visit, Visitor};

use crate::keys::{Principal, Scope};
use crate::ApiError;

/// Whether a key limited to tables may call `route` with `method` at all.
/// Each of these checks the tables it touches; the others are refused.
pub fn route_ok(method: &Method, route: &str) -> bool {
    let get = method == Method::GET || method == Method::HEAD;
    match route {
        // Whether the server is up, who the key is, and the cluster's shape.
        "/v1/health" | "/v1/whoami" | "/v1/cluster" => get,
        // The alerts that fire (ChloeCtx's "Seen before").
        "/v1/alerts" => get,
        // Tables: the listing and schemas show the tables it may read.
        "/v1/tables" | "/v1/schema" | "/v1/tables/:table/paths" => get,
        // Statements, by their tables.
        "/v1/query" | "/v1/sql" | "/v1/explain" => method == Method::POST,
        "/v1/query/cursors/:id" => method == Method::DELETE,
        // Rows, by their tables.
        "/v1/write" | "/v1/kv/get" | "/v1/import" => method == Method::POST,
        "/v1/kv/:table/:key" => get || method == Method::PUT || method == Method::DELETE,
        "/v1/export" => get,
        // Embeddings of text by a table's model, and the models that make
        // them (the fields of tables it reads).
        "/v1/embed" => method == Method::POST,
        // A connector's push: written with the key, so by its tables.
        "/v1/connectors/:name/push" => method == Method::POST,
        "/v1/models" => get,
        // A schema checked (no table is touched), and a plan, by its tables
        // ([`check_plan`]).
        "/v1/schema/check" | "/v1/plan" => method == Method::POST,
        // OTLP logs into a table it writes; a rank log it reads, exported.
        "/v1/logs" | "/v1/rank_log/export" | "/v1/rank_log/interleaving" => method == Method::POST,
        // Apps: the listing, and the apps that check a caller's tables
        // themselves ([`crate::apps`]; the others refuse it there).
        "/v1/apps" => get,
        "/v1/apps/:name/*rest" => true,
        // The command center's feedback, by its table ([`crate::feedback::may`]).
        "/v1/feedback" => get || method == Method::POST,
        "/v1/feedback/:id" => get || method == Method::PATCH || method == Method::DELETE,
        "/v1/feedback/:id/shot" | "/v1/feedback/:id/thumb" => get,
        // Keys of its own within its tables ([`crate::keys::Keys::create_delegated`]).
        "/v1/keys/own" => get || method == Method::POST,
        "/v1/keys/own/:id" => method == Method::DELETE,
        // The feed of the tables it may read (the server filters each line).
        "/v1/feed" => get,
        _ => false,
    }
}

/// Whether a connector's push key ([`Principal::push_only`]) may call
/// `route` with `method`: its push, the write the push makes with it, and
/// whether the server is up. Nothing that reads a table.
pub fn push_route_ok(method: &Method, route: &str) -> bool {
    let get = method == Method::GET || method == Method::HEAD;
    match route {
        "/v1/connectors/:name/push" | "/v1/write" => method == Method::POST,
        "/v1/health" | "/v1/whoami" => get,
        _ => false,
    }
}

/// Refuses (403) statement `text`, bound as `command`, if `who` (a key
/// limited to tables) may not run it: its kind's scope on the table it
/// changes, and read on every table it names. `tables` are the catalog's
/// tables, `functions` its stored functions. Anyone else may: their scope
/// was checked by the statement's kind.
pub fn check_statement(who: &Principal, text: &str, command: &Command, tables: &[String], functions: &FunctionSet) -> Result<(), ApiError> {
    if !who.limited() {
        return Ok(());
    }
    match command {
        Command::Query | Command::Explain { .. } => check_reads(who, text, tables, functions),
        // It lists only the tables the key reads ([`crate::service::Service::sql_in`]).
        Command::ShowTables => Ok(()),
        Command::Write { table, .. } => {
            who.may_table(Scope::Write, table, "a write")?;
            check_reads(who, text, tables, functions)
        }
        Command::CreateTable { schema, .. } => {
            who.may_table(Scope::Admin, &schema.table, "CREATE TABLE")?;
            reads(who, text, tables, functions, std::slice::from_ref(&schema.table))
        }
        Command::DropTable { tables: dropped, .. } => {
            for t in dropped {
                who.may_table(Scope::Admin, t, "DROP TABLE")?;
            }
            reads(who, text, tables, functions, dropped)
        }
        Command::AlterTable { table, .. } => {
            if let Some(t) = table {
                who.may_table(Scope::Admin, t, "ALTER TABLE")?;
            }
            check_reads(who, text, tables, functions)
        }
        Command::CreateRankProfile { table, .. } => who.may_table(Scope::Admin, table, "CREATE RANK PROFILE"),
        Command::DropRankProfile { table: Some(t), .. } => who.may_table(Scope::Admin, t, "DROP RANK PROFILE"),
        Command::DropRankProfile { table: None, .. } => Err(ApiError { code: None, status: 403, message: "a key limited to tables names the table of the rank profile it drops: DROP RANK PROFILE name ON table".into() }),
        Command::Function(f) => check_function(who, f, tables, functions),
    }
}

/// Refuses (403) `CREATE FUNCTION` `f` by `who`, a key limited to tables,
/// unless it is a key with the admin scope on some tables (an app's) that
/// defines a function no one has, or the one it defined again (the same
/// definition, as an app installs its functions at each start), whose
/// body reads only tables it may read and calls no model. It replaces
/// none, and drops none: those stay an admin's of the server, since a
/// stored function is every client's.
fn check_function(who: &Principal, f: &chloedb::query::function::Statement, tables: &[String], functions: &FunctionSet) -> Result<(), ApiError> {
    use chloedb::query::function::{Kind, Statement};
    let Statement::Create { def, at: None, .. } = f else {
        return who.may_server("DROP FUNCTION, a version made from another or AT VERSION");
    };
    if !who.tables.as_deref().unwrap_or_default().iter().any(|g| g.scope == Scope::Admin) {
        return who.may_server("a stored function, without the admin scope on any table,");
    }
    if let Some(cur) = functions.current(&def.name) {
        if cur.hash != def.hash() {
            return Err(ApiError {
                code: None, status: 403,
                message: format!("function {} exists (version {}): a key limited to tables defines new functions, and the same ones again, but replaces none (an admin key does); the key {:?} is limited to tables", def.name, cur.version, who.name),
            });
        }
    }
    // Its body, as a query reads it: what it reads, and no llm().
    let body = match def.kind {
        Kind::Table => def.body.clone(),
        Kind::Scalar => format!("SELECT {}", def.body),
    };
    check_reads(who, &body, tables, functions).map_err(|e| ApiError { message: format!("the body of {}: {}", def.name, e.message), ..e })
}

/// Refuses (403) a query or statement `text` that names a table `who` may
/// not read, or that reads tables its names do not show ([`self`]).
pub fn check_reads(who: &Principal, text: &str, tables: &[String], functions: &FunctionSet) -> Result<(), ApiError> {
    reads(who, text, tables, functions, &[])
}

/// [`check_reads`], but for the tables `made` a statement creates or
/// drops, checked by their statement's kind.
fn reads(who: &Principal, text: &str, tables: &[String], functions: &FunctionSet, made: &[String]) -> Result<(), ApiError> {
    if !who.limited() {
        return Ok(());
    }
    let refuse = |m: String| ApiError { code: None, status: 403, message: format!("{m}; the key {:?} is limited to tables", who.name) };
    // A query that packs its rows reads what the query without its clause
    // reads: FOLLOW reads the query's own table, and the clause names
    // columns of it, no table.
    let unpacked;
    let text = match chloedb::query::pack::split(text) {
        Ok(Some((rest, _))) => {
            unpacked = rest;
            unpacked.as_str()
        }
        _ => text,
    };
    let statements = chloedb::query::parse::parse(text).map_err(|e| ApiError::bad(e.to_string()))?;
    let mut walk = Walk::default();
    for s in &statements {
        walk_statement(s, functions, &mut walk)?;
    }
    if let Some(m) = walk.refused {
        return Err(refuse(m));
    }
    let catalog: BTreeSet<String> = tables.iter().map(|t| t.to_ascii_lowercase()).collect();
    if let Some(c) = walk.ctes.iter().find(|c| catalog.contains(*c)) {
        return Err(refuse(format!("WITH {c} names a table too: name it otherwise")));
    }
    // `recommend(...)` reads its recommender's counts table.
    let recommenders = chloedb::query::recommend::names_sql(text).map_err(refuse)?;
    for n in &recommenders {
        who.may_table(Scope::Read, &chloedb::query::recommend::counts_table(n), "recommend()")?;
    }
    for r in &walk.relations {
        if walk.ctes.contains(r) || made.iter().any(|m| m.eq_ignore_ascii_case(r)) {
            continue;
        }
        if r == chloedb::query::recommend::NAME && !recommenders.is_empty() {
            continue;
        }
        who.may_table(Scope::Read, r, "a query")?;
        // A name its grants cover that is not a table: as the engine says it
        // (a client tells a missing table by these words), but without the
        // engine's list of the database's tables, which names tables the
        // key may not see. It is refused rather than run, since a name of
        // no table may be a table function's, whose body names tables its
        // call does not.
        if !catalog.contains(r) {
            return Err(ApiError::bad(format!("no such table: {r}")));
        }
    }
    for a in walk.arguments.iter().filter(|a| catalog.contains(*a)) {
        who.may_table(Scope::Read, a, "a walk over an edge table")?;
    }
    for c in &walk.calls {
        let base = c.split('@').next().unwrap_or(c);
        if base == chloedb::query::llm::NAME {
            return Err(refuse("a query that calls llm() runs SQL a model writes, which no key limited to tables may".into()));
        }
        if base == "encode" {
            return Err(refuse("encode() reads a sequence model's own table".into()));
        }
        if functions.calls_llm() && functions.current(base).is_some() {
            return Err(refuse(format!("{base}() is a stored function, and this database's functions call llm()")));
        }
    }
    Ok(())
}

/// Whether statement `text` calls a function `functions` holds as a
/// stored one: a coordinator learns its functions again before judging
/// such a call ([`check_reads`]), since another coordinator may have
/// changed its body (audit N-D5).
pub fn calls_stored(text: &str, functions: &FunctionSet) -> bool {
    let Ok(statements) = chloedb::query::parse::parse(text) else { return false };
    let mut walk = Walk::default();
    for s in &statements {
        let _ = s.visit(&mut walk);
    }
    walk.calls.iter().any(|c| functions.current(c.split('@').next().unwrap_or(c)).is_some())
}

/// Walks statement `s`, a query with its stored functions written out
/// ([`chloedb::query::function::expand`], as the engine runs it), so that
/// a table function is checked by the tables its body reads.
fn walk_statement(s: &sql::Statement, functions: &FunctionSet, walk: &mut Walk) -> Result<(), ApiError> {
    let q = match s {
        sql::Statement::Query(q) => q,
        sql::Statement::Explain { statement, .. } => return walk_statement(statement, functions, walk),
        other => {
            let _ = other.visit(walk);
            return Ok(());
        }
    };
    match chloedb::query::function::expand(q, functions).map_err(|e| ApiError::bad(e.to_string()))? {
        Some((expanded, _)) => drop(expanded.visit(walk)),
        None => drop(q.visit(walk)),
    }
    Ok(())
}

/// Refuses (403) plan `plan` ([`chloedb::query::ir`], `POST /v1/plan`) if
/// `who` (a key limited to tables) may not run it: write on the table each
/// of its writes writes, and read on every table it reads, in subqueries
/// too, and on an edge table a walk follows. A sequence model's `encode`
/// reads its own table, and is refused as `encode()` is in SQL.
pub fn check_plan(who: &Principal, plan: &chloedb::query::ir::Plan) -> Result<(), ApiError> {
    use chloedb::query::ir::{Expr, Node};
    if !who.limited() {
        return Ok(());
    }
    #[derive(Default)]
    struct Seen {
        reads: BTreeSet<String>,
        writes: BTreeSet<String>,
        refused: Option<String>,
    }
    fn nodes(n: &Node, seen: &mut Seen) {
        match n {
            Node::Search(s) => drop(seen.reads.insert(s.table.to_ascii_lowercase())),
            Node::Insert { table, .. } | Node::Update { table, .. } | Node::Delete { table, .. } => drop(seen.writes.insert(table.to_ascii_lowercase())),
            _ => {}
        }
        for i in n.inputs() {
            nodes(i, seen);
        }
    }
    let mut seen = Seen::default();
    nodes(&plan.root, &mut seen);
    // The plans of subqueries, at any depth: their expressions are visited
    // with the plan's.
    let mut inner: Vec<Node> = Vec::new();
    plan.root.visit_exprs(&mut |e| match e {
        Expr::Subquery(n) | Expr::Values(n) | Expr::Keys(n) | Expr::Exists(n) | Expr::Scalar(n) => inner.push((**n).clone()),
        Expr::Graph(g) | Expr::Ppr(g) => {
            if let Some(t) = &g.edges {
                seen.reads.insert(t.to_ascii_lowercase());
            }
        }
        Expr::Encode { .. } => seen.refused = Some("a plan's encode reads a sequence model's own table".into()),
        _ => {}
    });
    for n in &inner {
        nodes(n, &mut seen);
    }
    if let Some(m) = seen.refused {
        return Err(ApiError { code: None, status: 403, message: format!("{m}; the key {:?} is limited to tables", who.name) });
    }
    for t in &seen.writes {
        who.may_table(Scope::Write, t, "a write plan")?;
    }
    for t in &seen.reads {
        who.may_table(Scope::Read, t, "a plan")?;
    }
    Ok(())
}

/// What statement `text` names. A `PACK n TOKENS` clause, which ChloeDB's
/// parser does not take ([`chloedb::query::pack::split`]), is read without:
/// it names no table (its `DEDUPE ... ON` names a field of the query's).
fn walk(text: &str) -> Result<Walk, ApiError> {
    let packed = chloedb::query::pack::split(text).map_err(|e| ApiError::bad(e.to_string()))?;
    let text = packed.as_ref().map_or(text, |(rest, _)| rest.as_str());
    let statements = chloedb::query::parse::parse(text).map_err(|e| ApiError::bad(e.to_string()))?;
    let mut walk = Walk::default();
    for s in &statements {
        let _ = s.visit(&mut walk);
    }
    Ok(walk)
}

/// The tables statement `text` reads or writes by name (`FROM`, `JOIN`,
/// subqueries, the table of a write), in lowercase and without its `WITH`
/// tables; none when it does not parse. What a parameter with no table
/// (`{"embed": ..}`, `{"vector_of": ..}`) resolves against.
pub fn named_tables(text: &str) -> Vec<String> {
    match walk(text) {
        Ok(w) => w.relations.iter().filter(|r| !w.ctes.contains(*r)).cloned().collect(),
        Err(_) => Vec::new(),
    }
}

/// What a statement names, as [`check_reads`] needs it.
#[derive(Default)]
struct Walk {
    /// Every table a `FROM`, `JOIN`, `INSERT`, `UPDATE`, `DELETE` or DDL
    /// names, in lowercase.
    relations: BTreeSet<String>,
    /// The `WITH` tables the statement defines.
    ctes: BTreeSet<String>,
    /// The functions it calls.
    calls: BTreeSet<String>,
    /// Names given as a call's arguments (an edge table a walk follows).
    arguments: BTreeSet<String>,
    /// Why it cannot be checked by its names.
    refused: Option<String>,
}

/// One name of a single part, in lowercase; `None` for `a.b`.
fn one_name(n: &sql::ObjectName) -> Option<String> {
    match n.0.as_slice() {
        [p] => p.as_ident().map(|i| i.value.to_ascii_lowercase()),
        _ => None,
    }
}

impl Visitor for Walk {
    type Break = ();

    fn pre_visit_query(&mut self, q: &sql::Query) -> ControlFlow<()> {
        if let Some(with) = &q.with {
            for c in &with.cte_tables {
                self.ctes.insert(c.alias.name.value.to_ascii_lowercase());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_relation(&mut self, r: &sql::ObjectName) -> ControlFlow<()> {
        match one_name(r) {
            Some(n) => drop(self.relations.insert(n)),
            None => self.refused = Some(format!("{r} is a name in parts: name a table by its name alone")),
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, t: &sql::TableFactor) -> ControlFlow<()> {
        match t {
            sql::TableFactor::Table { args: None, .. } | sql::TableFactor::Derived { .. } | sql::TableFactor::NestedJoin { .. } => {}
            sql::TableFactor::Table { name, .. } => self.refused = Some(format!("{name}(...) is a table function")),
            other => self.refused = Some(format!("{other} reads rows a key limited to tables cannot be checked for")),
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &sql::Expr) -> ControlFlow<()> {
        if let sql::Expr::Function(f) = e {
            let name = f.name.0.iter().filter_map(|p| p.as_ident()).map(|i| i.value.to_ascii_lowercase()).collect::<Vec<_>>().join(".");
            self.calls.insert(name);
            if let sql::FunctionArguments::List(list) = &f.args {
                for a in &list.args {
                    let e = match a {
                        sql::FunctionArg::Unnamed(e) | sql::FunctionArg::Named { arg: e, .. } | sql::FunctionArg::ExprNamed { arg: e, .. } => e,
                    };
                    if let sql::FunctionArgExpr::Expr(sql::Expr::Identifier(i)) = e {
                        self.arguments.insert(i.value.to_ascii_lowercase());
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Grant;
    use serde_json::{json, Value as Json};

    fn key(grants: &[&str]) -> Principal {
        let g: Vec<Grant> = grants.iter().map(|g| Grant::parse(g).unwrap()).collect();
        let scope = g.iter().map(|g| g.scope).max().unwrap();
        Principal { name: "app:test".into(), key: Some("k".into()), scope, root: false, tables: Some(g.into()), role: None, agent: false, on_behalf_of: None, allow: None }
    }

    fn tables() -> Vec<String> {
        ["crawl_docs", "crawl_links", "secrets", "sa_people", "edges_secret"].map(String::from).to_vec()
    }

    fn reads(who: &Principal, sql: &str) -> Result<(), ApiError> {
        check_reads(who, sql, &tables(), &FunctionSet::default())
    }

    #[test]
    fn a_limited_key_reads_only_the_tables_it_names() {
        let k = key(&["crawl_*=admin"]);
        assert!(reads(&k, "SELECT * FROM crawl_docs WHERE id = 1").is_ok());
        assert!(reads(&k, "SELECT d.id FROM crawl_docs d JOIN crawl_links l ON l.src = d.id").is_ok());
        assert!(reads(&k, "SELECT * FROM CRAWL_DOCS").is_ok());
        assert!(reads(&k, "WITH x AS (SELECT * FROM crawl_docs) SELECT * FROM x").is_ok());
        // A missing table its grants cover is missing, as for any key, and
        // the answer lists no other table; one they do not cover is refused
        // whether or not it exists.
        let missing = reads(&k, "SELECT * FROM crawl_pages").unwrap_err();
        assert_eq!((missing.status, missing.message.as_str()), (400, "no such table: crawl_pages"));
        assert_eq!(reads(&k, "SELECT * FROM nowhere").unwrap_err().status, 403);
        for bad in [
            "SELECT * FROM secrets",
            "SELECT * FROM \"secrets\"",
            "SELECT * FROM Secrets",
            "SELECT * FROM crawl_docs, secrets",
            "SELECT * FROM crawl_docs JOIN secrets ON true",
            "SELECT * FROM crawl_docs WHERE id IN (SELECT id FROM secrets)",
            "SELECT (SELECT max(id) FROM secrets) FROM crawl_docs",
            "SELECT * FROM crawl_docs WHERE EXISTS (SELECT 1 FROM secrets)",
            "WITH x AS (SELECT * FROM secrets) SELECT * FROM x",
            "SELECT * FROM (SELECT * FROM secrets) s",
            "SELECT * FROM crawl_docs UNION ALL SELECT * FROM secrets",
            "SELECT * FROM main.crawl_docs",
            // A WITH table of a table's name could shadow it in one scope
            // and not in another.
            "SELECT * FROM secrets, (WITH secrets AS (SELECT 1) SELECT * FROM secrets) s",
            "WITH crawl_docs AS (SELECT 1) SELECT * FROM crawl_docs",
            // Table functions, llm(), encode().
            "SELECT * FROM recall('q')",
            "SELECT * FROM crawl_docs(1)",
            "SELECT llm('say', body) FROM crawl_docs",
            "SELECT encode(m, items) FROM crawl_docs",
            // An edge table a walk names.
            "SELECT id FROM crawl_docs WHERE REACHABLE(edges_secret, 1)",
        ] {
            let e = reads(&k, bad).expect_err(bad);
            assert_eq!(e.status, 403, "{bad}: {}", e.message);
        }
        // Not a table at all: refused too (it might be a function).
        assert_eq!(reads(&k, "SELECT * FROM nothing_here").unwrap_err().status, 403);
        // PACK is judged by the query without its clause (audit N-D6).
        assert!(reads(&k, "SELECT id, body FROM crawl_docs ORDER BY bm25(body, 'x') DESC PACK 4000 TOKENS DEDUPE 0.9 FOLLOW links 1 HOP WITH RECEIPT LIMIT 50").is_ok());
        assert_eq!(reads(&k, "SELECT id, body FROM secrets ORDER BY id PACK 4000 TOKENS").unwrap_err().status, 403);
        assert_eq!(reads(&k, "SELECT id FROM crawl_docs WHERE id IN (SELECT id FROM secrets) PACK 100 TOKENS").unwrap_err().status, 403);
        // A key of every table is not checked here.
        let all = Principal { name: "ops".into(), key: None, scope: Scope::Admin, root: false, tables: None, role: None, agent: false, on_behalf_of: None, allow: None };
        assert!(reads(&all, "SELECT * FROM secrets").is_ok());
    }

    /// The schema of a table `name (id KEYWORD PRIMARY KEY)`.
    fn schema(name: &str) -> chloedb::schema::Schema {
        match chloedb::query::sql::bind_command(&format!("CREATE TABLE {name} (id KEYWORD PRIMARY KEY, body TEXT)"), &[], &Default::default()).unwrap() {
            Command::CreateTable { schema, .. } => schema,
            _ => unreachable!(),
        }
    }

    #[test]
    fn read_grants_and_write_grants() {
        let k = key(&["sheets_*=admin", "*=read"]);
        assert!(reads(&k, "SELECT * FROM secrets JOIN crawl_docs ON true").is_ok());
        let functions = FunctionSet::default();
        let (secrets, sheets_x) = (schema("secrets"), schema("sheets_x"));
        let catalog = [&secrets, &sheets_x];
        let mut names = tables();
        names.push("sheets_x".into());
        let check = |who: &Principal, sql: &str| {
            let command = chloedb::query::sql::bind_command(sql, &catalog, &Default::default()).unwrap();
            check_statement(who, sql, &command, &names, &functions)
        };
        // Statements by their kind: CREATE needs admin on the table.
        assert!(check(&k, "CREATE TABLE sheets_books (id KEYWORD PRIMARY KEY)").is_ok());
        assert_eq!(check(&k, "CREATE TABLE other (id KEYWORD PRIMARY KEY)").unwrap_err().status, 403);
        assert_eq!(check(&k, "DROP TABLE secrets").unwrap_err().status, 403);
        assert!(check(&k, "DROP TABLE sheets_x").is_ok());
        let e = check(&k, "DELETE FROM secrets").unwrap_err();
        assert!(e.status == 403 && e.message.contains("write"), "{}", e.message);
        assert!(check(&k, "DELETE FROM sheets_x WHERE id = 'a'").is_ok());
        // A write of its own table that reads one it may not.
        let r = key(&["sheets_*=admin"]);
        assert!(check(&r, "UPDATE sheets_x SET body = 'b' WHERE id = 'a'").is_ok());
        // (The binder takes no such subquery yet; its names are refused
        // first all the same.)
        assert_eq!(check_reads(&r, "DELETE FROM sheets_x WHERE id IN (SELECT id FROM secrets)", &names, &functions).unwrap_err().status, 403);
        // Read on a table is not write there.
        let w = key(&["sheets_*=read"]);
        assert_eq!(check(&w, "DELETE FROM sheets_x").unwrap_err().status, 403);
        // A function: a key with admin on some tables defines a new one;
        // one with read alone does not.
        let f = "CREATE FUNCTION twice(x FLOAT) RETURNS FLOAT AS :x * 2";
        let command = chloedb::query::sql::bind_command(f, &catalog, &Default::default()).unwrap();
        assert!(check_statement(&key(&["*=admin"]), f, &command, &names, &functions).is_ok());
        assert_eq!(check_statement(&key(&["*=read"]), f, &command, &names, &functions).unwrap_err().status, 403);
    }

    /// The function set of these definitions.
    fn defined(sqls: &[&str]) -> FunctionSet {
        use chloedb::query::function::{change, parse_statement, Change};
        let mut set = FunctionSet::default();
        for s in sqls {
            let stmt = parse_statement(s).unwrap().unwrap();
            if let Change::Put(f) = change(&set, &stmt, None, Some("admin")).unwrap() {
                set = FunctionSet::of(set.all().map(|f| (**f).clone()).chain([f]));
            }
        }
        set
    }

    #[test]
    fn stored_functions_and_a_limited_key() {
        let k = key(&["crawl_*=admin", "sa_numbers=read"]);
        let mut names = tables();
        names.push("sa_numbers".into());
        let set = defined(&[
            "CREATE FUNCTION days(n INT DEFAULT 7) RETURNS TABLE AS SELECT id FROM sa_numbers WHERE id < :n LIMIT 100",
            "CREATE FUNCTION leak() RETURNS TABLE AS SELECT email AS id FROM sa_people LIMIT 10",
            "CREATE FUNCTION twice(x FLOAT) RETURNS FLOAT AS :x * 2",
        ]);
        let reads = |sql: &str| check_reads(&k, sql, &names, &set);
        // A table function is checked by what its body reads.
        assert!(reads("SELECT * FROM days(3)").is_ok(), "{:?}", reads("SELECT * FROM days(3)"));
        assert!(reads("SELECT d.id FROM crawl_docs d JOIN days(3) n ON n.id = d.id").is_ok());
        assert_eq!(reads("SELECT * FROM leak()").unwrap_err().status, 403);
        assert_eq!(reads("SELECT d.id FROM crawl_docs d WHERE d.id IN (SELECT id FROM leak())").unwrap_err().status, 403);
        assert!(reads("SELECT twice(2) AS x FROM crawl_docs").is_ok());
        // Not a stored one: refused, as before.
        assert_eq!(reads("SELECT * FROM recall('q')").unwrap_err().status, 403);
        // Defining: new, or the same again; never another's in its place.
        let create = |sql: &str| {
            let stmt = chloedb::query::function::parse_statement(sql).unwrap().unwrap();
            check_statement(&k, sql, &Command::Function(stmt), &names, &set)
        };
        assert!(create("CREATE OR REPLACE FUNCTION twice(x FLOAT) RETURNS FLOAT AS :x * 2").is_ok());
        assert_eq!(create("CREATE OR REPLACE FUNCTION twice(x FLOAT) RETURNS FLOAT AS :x * 3").unwrap_err().status, 403);
        assert!(create("CREATE FUNCTION mine(n INT DEFAULT 2) RETURNS TABLE AS SELECT id FROM crawl_docs LIMIT :n").is_ok());
        assert!(create("CREATE FUNCTION thrice(x FLOAT) RETURNS FLOAT AS :x * 3").is_ok());
        assert_eq!(create("CREATE FUNCTION peek() RETURNS TABLE AS SELECT email AS id FROM sa_people LIMIT 1").unwrap_err().status, 403);
        assert_eq!(create("CREATE FUNCTION ask(x TEXT) RETURNS TEXT AS llm('m', 'say', :x)").unwrap_err().status, 403);
        assert_eq!(create("DROP FUNCTION twice").unwrap_err().status, 403);
    }

    #[test]
    fn pack_is_read_without_its_clause() {
        let k = key(&["crawl_*=admin"]);
        assert!(reads(&k, "SELECT id FROM crawl_docs ORDER BY bm25(body, 'x') DESC PACK 200 TOKENS").is_ok());
        assert!(reads(&k, "SELECT id FROM crawl_docs ORDER BY bm25(body, 'x') DESC PACK 200 TOKENS DEDUPE 0.95 ON emb WITH RECEIPT LIMIT 5").is_ok());
        assert_eq!(reads(&k, "SELECT id FROM secrets ORDER BY bm25(body, 'x') DESC PACK 200 TOKENS").unwrap_err().status, 403);
        assert_eq!(reads(&k, "SELECT id FROM crawl_docs WHERE id IN (SELECT id FROM secrets) PACK 20 TOKENS").unwrap_err().status, 403);
        assert_eq!(named_tables("SELECT id FROM crawl_docs ORDER BY bm25(body, 'x') DESC PACK 200 TOKENS"), vec!["crawl_docs".to_string()]);
        assert_eq!(named_tables("WITH x AS (SELECT * FROM a) SELECT * FROM x JOIN B ON true"), vec!["a".to_string(), "b".to_string()]);
        assert!(named_tables("not sql").is_empty());
    }

    #[test]
    fn plans_by_their_tables() {
        use chloedb::query::ir::Plan;
        let k = key(&["crawl_*=write", "public=read"]);
        let plan = |j: Json| Plan::from_json(&j).unwrap();
        let search = |t: &str| json!({"node": "search", "table": t});
        assert!(check_plan(&k, &plan(json!({"version": 1, "root": search("crawl_docs")}))).is_ok());
        assert!(check_plan(&k, &plan(json!({"version": 1, "root": search("public")}))).is_ok());
        assert_eq!(check_plan(&k, &plan(json!({"version": 1, "root": search("secrets")}))).unwrap_err().status, 403);
        // A write of a table it may only read.
        assert_eq!(check_plan(&k, &plan(json!({"version": 1, "root": {"node": "delete", "table": "public", "input": search("public")}}))).unwrap_err().status, 403);
        assert!(check_plan(&k, &plan(json!({"version": 1, "root": {"node": "delete", "table": "crawl_docs", "input": search("crawl_docs")}}))).is_ok());
        // A table read in a subquery.
        let sub = plan(json!({"version": 1, "root": {"node": "filter", "input": search("crawl_docs"), "cond": {"exists": search("secrets")}}}));
        assert_eq!(check_plan(&k, &sub).unwrap_err().status, 403);
        // Not checked for a key of every table.
        let all = Principal { name: "ops".into(), key: None, scope: Scope::Admin, root: false, tables: None, role: None, agent: false, on_behalf_of: None, allow: None };
        assert!(check_plan(&all, &plan(json!({"version": 1, "root": search("secrets")}))).is_ok());
    }

    #[test]
    fn routes_of_a_limited_key() {
        assert!(route_ok(&Method::POST, "/v1/sql"));
        assert!(route_ok(&Method::GET, "/v1/tables"));
        assert!(route_ok(&Method::PUT, "/v1/kv/:table/:key"));
        assert!(route_ok(&Method::GET, "/v1/feed"));
        for (m, r) in [(Method::POST, "/v1/plan"), (Method::POST, "/v1/schema/check"), (Method::POST, "/v1/logs"), (Method::GET, "/v1/apps"), (Method::GET, "/v1/apps/:name/*rest"), (Method::POST, "/v1/apps/:name/*rest"), (Method::GET, "/v1/export")] {
            assert!(route_ok(&m, r), "{m} {r}");
        }
        for (m, r) in [(Method::POST, "/v1/admin/keys"), (Method::POST, "/v1/admin/shards"), (Method::GET, "/v1/feed/promoted"), (Method::POST, "/v1/feed/keys"), (Method::POST, "/v1/mcp"), (Method::GET, "/v1/logs"), (Method::GET, "/v1/events"), (Method::GET, "/v1/health/fleet"), (Method::GET, "/v1/stats"), (Method::GET, "/v1/queries/recent"), (Method::POST, "/v1/alerts/:rule/ack"), (Method::GET, "/metrics")] {
            assert!(!route_ok(&m, r), "{m} {r}");
        }
    }
}
