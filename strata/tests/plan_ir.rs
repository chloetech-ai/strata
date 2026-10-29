//! The query plan (`strata::query::ir`): every kind of statement the
//! binder takes makes a plan that survives JSON and gives back the
//! statement it was made of; plans in JSON carry types, versions and
//! features; plans made elsewhere run as the SQL they stand for; and
//! `EXPLAIN` shows them.

use std::path::Path;

use strata::db::{Db, SqlResult};
use strata::query::ir::Plan;
use strata::query::{bind, bind_command, Command, Params};
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

const PAPERS: &str = "CREATE TABLE papers (
    id KEYWORD PRIMARY KEY,
    title TEXT OPTIONS(analyzer = 'simple'),
    body TEXT,
    category KEYWORD,
    tags KEYWORD[],
    year INT,
    refs INT[],
    rating FLOAT,
    done BOOL,
    published TIMESTAMP,
    emb VECTOR(4) OPTIONS(index = 'flat'),
    terms SPARSE,
    cites EDGES
)";

fn db(path: &Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    run(&db, PAPERS);
    db
}

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &params()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn params() -> Params {
    Params::new().set("v", Value::Vector(vec![0.1, 0.2, 0.3, 0.4].into())).set("ids", Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]))
}

/// SELECTs of one table, one of each kind the binder takes.
const SELECTS: &[&str] = &[
    "SELECT * FROM papers",
    "SELECT id, title FROM papers WHERE MATCH(body, 'graph neural') AND year >= 2019 ORDER BY bm25(body, 'graph neural') DESC LIMIT 10",
    "SELECT id, score() AS s FROM papers WHERE MATCH(title, 'Graph', fuzzy => 'auto', synonyms => 1) ORDER BY bm25(body, 'graph', fuzzy => 1) DESC LIMIT 5 OFFSET 2",
    "SELECT id FROM papers WHERE MATCH(body, '\"graph neural\" OR (deep AND NOT shallow)')",
    "SELECT id FROM papers WHERE year BETWEEN 2000 AND 2010 AND rating > 0.5 AND NOT (category = 'ml') AND category IN ('a', 'b', 'c')",
    "SELECT id FROM papers WHERE NOT (year > 5 OR rating <= 1.5) OR tags = 'x' OR NOT (refs = 3)",
    "SELECT id FROM papers WHERE year IS NULL OR rating IS NOT NULL OR category IS NULL",
    "SELECT id FROM papers WHERE rating = 2.5 OR rating != 3 OR year != 7 OR year < 1e30 OR year > 9223372036854775807",
    "SELECT id FROM papers WHERE done AND published >= '2026-10-01' AND published < '2026-11-01T00:00:00Z'",
    "SELECT id FROM papers WHERE NOT done OR done IS NOT TRUE OR done IS FALSE",
    "SELECT id FROM papers WHERE category IN (:ids) AND TRUE",
    "SELECT id FROM papers WHERE FALSE",
    "SELECT id, rating FROM papers ORDER BY rating DESC NULLS FIRST, id LIMIT 3",
    "SELECT id FROM papers ORDER BY year, id DESC",
    "SELECT id FROM papers ORDER BY similarity(emb, :v) DESC LIMIT 5",
    "SELECT id FROM papers ORDER BY distance(emb, '[0.1, 0.2, 0.3, 0.4]') ASC LIMIT 5",
    "SELECT id FROM papers ORDER BY rrf(bm25(body, 'graph'), similarity(emb, :v), k => 10, depth => 50) DESC LIMIT 5",
    "SELECT id FROM papers ORDER BY 0.3 * bm25(body, 'graph') + 0.7 * similarity(emb, :v) - rating DESC LIMIT 5",
    "SELECT id FROM papers ORDER BY bm25(body, 'graph') * decay(published, now(), 2592000) DESC LIMIT 5",
    "SELECT id FROM papers ORDER BY decay(year, 2020, 5, offset => 1, decay => 0.3, shape => 'gauss') DESC",
    "SELECT id FROM papers ORDER BY similarity(terms, '{\"graph\": 1.5, \"net\": 0.5}') DESC LIMIT 3",
    "SELECT id, hops() FROM papers WHERE REACHABLE(cites, 'p1', hops => 2, dir => 'both', labels => 'x,y', through => year > 2000) ORDER BY hops() LIMIT 20",
    "SELECT id FROM papers WHERE PATH(cites, 'p1', ('p5', 'p6'), max_hops => 4) ORDER BY hops() DESC",
    "SELECT id FROM papers WHERE NOT REACHABLE(cites, ('p1', 'p2'), hops => 0, min_hops => 0) AND REACHABLE(emb, 'p1', k => 5, min_sim => 0.5)",
    "SELECT id FROM papers ORDER BY ppr(cites, (SELECT id FROM papers WHERE MATCH(body, 'graph') ORDER BY bm25(body, 'graph') DESC LIMIT 3), alpha => 0.2, weighted => 1) DESC LIMIT 10",
    "SELECT category, COUNT(*) FROM papers GROUP BY category",
    "SELECT category, COUNT(*) AS n, AVG(rating), MAX(published) FROM papers WHERE year > 2000 GROUP BY category HAVING COUNT(*) >= 2 AND AVG(rating) < 3 ORDER BY n DESC LIMIT 5",
    "SELECT category, year, SUM(rating) FROM papers GROUP BY category, year ORDER BY SUM(rating) DESC, category LIMIT 10",
    "SELECT COUNT(*), COUNT(DISTINCT category), MIN(year), COUNT(rating) FROM papers WHERE MATCH(body, 'graph')",
    "SELECT category, COUNT(*) FROM papers GROUP BY category HAVING MAX(published) > '2026-01-01' ORDER BY category DESC",
    "SELECT tags, COUNT(*) FROM papers GROUP BY tags HAVING NOT (COUNT(*) BETWEEN 1 AND 2) OR COUNT(*) != 7 ORDER BY 2",
    "SELECT year, COUNT(*) FROM papers GROUP BY year ORDER BY year LIMIT 3 OFFSET 1",
    "SELECT category, COUNT(*) FROM papers GROUP BY category HAVING category = 'ml' OR category != 'x'",
];

/// Writes of each kind.
const WRITES: &[&str] = &[
    "INSERT INTO papers (id, title, year, done, published, tags, emb, terms, cites) VALUES ('n1', 'a', 1, TRUE, '2026-10-01', ARRAY['x', 'y'], ARRAY[0.1, 0.2, 0.3, 0.4], '{\"a\": 1}', '[\"p1\"]'), ('n2', 'b', NULL, FALSE, NULL, ARRAY['z'], NULL, NULL, NULL)",
    "INSERT OR REPLACE INTO papers (id, year) VALUES ('p1', 2)",
    "INSERT INTO papers (id, year) VALUES ('p1', 2) ON CONFLICT (id) DO NOTHING",
    "INSERT INTO papers (id, year, rating) VALUES ('p1', 2, 0.5) ON CONFLICT (id) DO UPDATE SET year = excluded.year, rating = rating + 1",
    "UPDATE papers SET year = year + 1, done = FALSE WHERE MATCH(body, 'graph') LIMIT 5",
    "UPDATE papers SET rating = rating - 0.5, year = year - 2 WHERE category = 'ml'",
    "UPDATE papers SET published = '2026-01-01T10:00:00Z', tags = ARRAY['a'] WHERE id IN ('p1', 'p2')",
    "DELETE FROM papers WHERE year < 2000",
    "DELETE FROM papers WHERE done ORDER BY year DESC LIMIT 3",
    // SET to expressions of the row.
    "UPDATE papers SET rating = rating * 2 + year, title = lower(coalesce(title, 'untitled')) || '!', done = NOT done WHERE category = 'db'",
    "INSERT INTO papers (id, year, rating) VALUES ('p2', 3, 1.5) ON CONFLICT (id) DO UPDATE SET year = CASE WHEN excluded.year > year THEN excluded.year ELSE year END, rating = coalesce(rating, 0) + excluded.rating",
    "DELETE FROM papers WHERE year > - -2024",
];

/// The plan of `sql`, through JSON and back.
fn through_json(plan: &Plan) -> Plan {
    let j = plan.to_json().unwrap();
    let back = Plan::from_json(&j).unwrap_or_else(|e| panic!("{e}: {j}"));
    assert_eq!(&back, plan, "{j}");
    // And through JSON text.
    assert_eq!(Plan::from_json(&serde_json::from_str(&j.to_string()).unwrap()).unwrap(), back);
    back
}

#[test]
fn every_statement_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"));
    let schema = db.storage().table("papers").unwrap();
    for sql in SELECTS {
        let select = bind(sql, &schema, &params()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let plan = through_json(&Plan::of_select(&schema, &select));
        assert_eq!(plan.to_select(&schema).unwrap_or_else(|e| panic!("{sql}: {e}")), select, "{sql}");
        // Binding again takes another now().
        if !sql.contains("now()") {
            assert_eq!(db.explain(sql, &params()).unwrap(), Plan::of_select(&schema, &select), "{sql}");
        }
    }
    for sql in WRITES {
        let Command::Write { write, .. } = bind_command(sql, &[&schema], &params()).unwrap_or_else(|e| panic!("{sql}: {e}")) else { panic!("{sql}") };
        let plan = through_json(&Plan::of_write(&schema, &write));
        assert_eq!(plan.to_write(&schema).unwrap_or_else(|e| panic!("{sql}: {e}")), write, "{sql}");
    }
}

#[test]
fn json_holds_typed_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"));
    let j = |sql: &str| db.explain(sql, &params()).unwrap().to_json_text().unwrap();
    let s = j("SELECT id FROM papers WHERE done AND published >= '2026-10-01' AND year = 5 AND category = '5' AND rating = 5");
    for want in [r#"{"bool":true}"#, r#"{"timestamp":1790812800000000}"#, r#"{"int":5}"#, r#"{"text":"5"}"#, r#"{"float":5.0}"#] {
        assert!(s.contains(want), "{want} in {s}");
    }
    assert!(s.starts_with(r#"{"version":1,"root":{"node":"project""#), "{s}");
    let s = j("INSERT INTO papers (id, done, published) VALUES ('a', FALSE, '2026-10-01T00:00:01Z')");
    assert!(s.contains(r#"{"bool":false}"#) && s.contains(r#"{"timestamp":1790812801000000}"#) && s.contains(r#""null""#), "{s}");
}

#[test]
fn newer_plans_are_refused_by_what_they_need() {
    let ok = serde_json::json!({"version": 1, "root": {"node": "project", "input": {"node": "search", "table": "papers"}, "columns": []}});
    Plan::from_json(&ok).unwrap();
    let refused = |j: serde_json::Value| Plan::from_json(&j).err().expect("refused").to_string();
    let mut v2 = ok.clone();
    v2["version"] = 2.into();
    let e = refused(v2);
    assert!(e.contains("plan version 2") && e.contains("newer"), "{e}");
    let mut feature = ok.clone();
    feature["features"] = serde_json::json!(["hash_join"]);
    let e = refused(feature);
    assert!(e.contains("\"hash_join\"") && e.contains("newer"), "{e}");
    let mut node = ok.clone();
    node["root"]["input"] = serde_json::json!({"node": "hash_join", "left": {}, "right": {}});
    assert!(refused(node).contains("hash_join"));
    assert!(refused(serde_json::json!({"root": {}})).contains("needs a version"));
    let mut nan = ok.clone();
    nan["root"]["input"]["filter"] = serde_json::json!({"lit": {"float": null}});
    refused(nan);
}

/// A plan written by hand, as a front-end other than SQL writes it, runs
/// as the SQL it stands for, with SQL's rules for NULL.
#[test]
fn plans_from_elsewhere_run_as_their_sql() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"));
    for i in 0..12 {
        let year = if i % 4 == 3 { "NULL".to_string() } else { (2015 + i).to_string() };
        let done = ["TRUE", "FALSE", "NULL"][i % 3];
        run(&db, &format!("INSERT INTO papers (id, body, year, category, done, published) VALUES ('p{i}', 'graph {i}', {year}, '{}', {done}, '2026-10-{:02}')", ["ml", "db"][i % 2], i + 1));
    }
    let schema = db.storage().table("papers").unwrap();
    let snap = db.storage().snapshot().table("papers").unwrap();
    let ids = |rs: strata::query::exec::ResultSet| {
        let mut v: Vec<String> = rs.rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
        v.sort();
        v
    };
    let field = |name: &str| serde_json::json!({"field": schema.field_index(name).unwrap()});
    let cmp = |op: &str, f: &str, v: serde_json::Value| serde_json::json!({"cmp": {"op": op, "left": field(f), "right": {"lit": v}}});
    let plan = |filter: serde_json::Value| {
        serde_json::json!({"version": 1, "root": {"node": "project", "columns": [{"name": "id", "expr": field("id")}],
            "input": {"node": "search", "table": "papers", "filter": filter}}})
    };
    let cases = [
        (cmp("ge", "year", serde_json::json!({"int": 2020})), "year >= 2020"),
        (serde_json::json!({"not": cmp("eq", "year", serde_json::json!({"int": 2016}))}), "NOT (year = 2016)"),
        (cmp("ne", "category", serde_json::json!({"text": "ml"})), "category != 'ml'"),
        (serde_json::json!({"not": {"and": [cmp("lt", "year", serde_json::json!({"float": 2019.5})), cmp("eq", "done", serde_json::json!({"bool": true}))]}}), "NOT (year < 2019.5 AND done)"),
        (cmp("lt", "published", serde_json::json!({"timestamp": 1_791_158_400_000_000i64})), "published < '2026-10-05'"),
        (serde_json::json!({"or": [{"is_null": field("year")}, {"in": {"expr": field("done"), "list": [{"lit": {"bool": false}}]}}]}), "year IS NULL OR done = FALSE"),
    ];
    for (filter, cond) in cases {
        let sql = format!("SELECT id FROM papers WHERE {cond}");
        let p = Plan::from_json(&plan(filter)).unwrap();
        let select = p.to_select(&schema).unwrap_or_else(|e| panic!("{cond}: {e}"));
        let want = ids(db.query(&sql, &Params::new()).unwrap());
        assert!(!want.is_empty() && want.len() < 12, "{cond}: {want:?}");
        assert_eq!(ids(db.execute(&snap, &select, &Default::default()).unwrap()), want, "{cond}");
    }
    // Shapes the executor does not run yet are refused, by name.
    let sorted = serde_json::json!({"version": 1, "root": {"node": "project", "columns": [],
        "input": {"node": "sort", "keys": [{"expr": field("year")}], "input": {"node": "search", "table": "papers"}}}});
    let e = Plan::from_json(&sorted).unwrap().to_select(&schema).err().unwrap().to_string();
    assert!(e.contains("Sort over a Search"), "{e}");
    let other = serde_json::json!({"version": 1, "root": {"node": "project", "columns": [], "input": {"node": "search", "table": "seed"}}});
    let e = Plan::from_json(&other).unwrap().to_select(&schema).err().unwrap().to_string();
    assert!(e.contains("table seed"), "{e}");
}

/// The rows of a few papers, the same in every database here.
fn fill(db: &Db) {
    for i in 0..12 {
        let year = if i % 4 == 3 { "NULL".to_string() } else { (2015 + i).to_string() };
        let done = ["TRUE", "FALSE", "NULL"][i % 3];
        let cites = if i > 0 { format!("'[\"p{}\"]'", i - 1) } else { "NULL".into() };
        run(db, &format!(
            "INSERT INTO papers (id, body, year, rating, category, tags, done, published, emb, cites) VALUES ('p{i}', 'graph {i}', {year}, {}, '{}', ARRAY['t{}'], {done}, '2026-10-{:02}', ARRAY[0.{i}, 0.2, 0.3, 0.4], {cites})",
            i as f64 / 4.0,
            ["ml", "db"][i % 2],
            i % 3,
            i + 1
        ));
    }
}

fn rows(r: SqlResult) -> Vec<Vec<Value>> {
    let SqlResult::Rows(r) = r else { panic!("rows") };
    r.rows
}

/// Db::run runs a plan as Db::sql runs its statement: every SELECT gives
/// the same rows, and every write leaves the same table, from a plan that
/// went through JSON.
#[test]
fn db_runs_plans() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (db(&dir.path().join("a.strata")), db(&dir.path().join("b.strata")));
    fill(&a);
    fill(&b);
    // The same rows, or the same error (a synonym list the database lacks).
    let mut answered = 0;
    for sql in SELECTS.iter().filter(|s| !s.contains("now()")) {
        let plan = through_json(&a.explain(sql, &params()).unwrap());
        let want = a.sql(sql, &params()).map(rows).map_err(|e| e.to_string());
        answered += want.is_ok() as usize;
        assert_eq!(a.run(&plan).map(rows).map_err(|e| e.to_string()), want, "{sql}");
    }
    assert!(answered + 2 >= SELECTS.len(), "{answered}");
    let all = |db: &Db| {
        let mut r = rows(run(db, "SELECT * FROM papers"));
        r.sort_by_key(|r| r[0].as_str().unwrap().to_string());
        r
    };
    let mut written = 0;
    for sql in WRITES {
        let plan = through_json(&b.explain(sql, &params()).unwrap());
        let (SqlResult::Written(x), SqlResult::Written(y)) = (run(&a, sql), b.run(&plan).unwrap_or_else(|e| panic!("{sql}: {e}"))) else { panic!("{sql}") };
        assert_eq!(x, y, "{sql}");
        assert_eq!(all(&a), all(&b), "{sql}");
        written += x;
    }
    assert!(written >= 20, "{written}");
    // A plan names its table, which must exist; a read is not prepared as
    // a write.
    let mut j = b.explain("DELETE FROM papers", &Params::new()).unwrap().to_json().unwrap();
    j["root"]["table"] = "nowhere".into();
    let e = b.run(&Plan::from_json(&j).unwrap()).err().unwrap().to_string();
    assert!(e.contains("nowhere"), "{e}");
    let read = b.explain("SELECT id FROM papers", &Params::new()).unwrap();
    assert!(b.prepare_plan(&read).err().unwrap().to_string().contains("reads"));
}

/// A SET of a plan is an expression of the row, checked as SQL's is.
#[test]
fn set_plans_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"));
    fill(&db);
    let schema = db.storage().table("papers").unwrap();
    let year = schema.field_index("year").unwrap();
    let j = db.explain("UPDATE papers SET year = year * 2 WHERE id = 'p1'", &Params::new()).unwrap().to_json().unwrap();
    let set = serde_json::json!({"arith": {"op": "mul", "left": {"field": year}, "right": {"lit": {"int": 2}}}});
    assert_eq!(j["root"]["sets"][0]["value"], set, "{j}");
    let with = |value: serde_json::Value| {
        let mut j = j.clone();
        j["root"]["sets"][0]["value"] = value;
        Plan::from_json(&j).unwrap()
    };
    for (value, want) in [
        (serde_json::json!("score"), "SET takes an expression of the row's fields"),
        (serde_json::json!({"call": {"func": "abs", "args": ["hops"]}}), "SET takes an expression of the row's fields"),
        (serde_json::json!({"arith": {"op": "add", "left": {"excluded": year}, "right": {"lit": {"int": 1}}}}), "excluded.year is the row an INSERT proposed"),
        (serde_json::json!({"neg": {"field": 99}}), "papers has no field 99"),
    ] {
        let e = db.run(&with(value.clone())).err().unwrap_or_else(|| panic!("{value} ran")).to_string();
        assert!(e.contains(want), "{value}: {e}");
    }
    assert!(matches!(db.run(&with(set)).unwrap(), SqlResult::Written(1)));
    assert_eq!(rows(run(&db, "SELECT year FROM papers WHERE id = 'p1'")), [[Value::Int(2 * 2016)]]);
}

fn lines(db: &Db, sql: &str) -> Vec<String> {
    let SqlResult::Rows(r) = run(db, sql) else { panic!("{sql}: rows") };
    assert_eq!(r.columns, ["plan"]);
    r.rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

#[test]
fn explain_shows_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"));
    assert_eq!(
        lines(&db, "EXPLAIN SELECT id, title, score() AS s FROM papers WHERE MATCH(body, 'graph neural') AND year >= 2019 AND NOT done ORDER BY bm25(body, 'graph') DESC LIMIT 10"),
        [
            "Project: id, title, score() AS s",
            "  Limit: 10",
            "    Search papers",
            "      filter: MATCH(body, 'graph neural') AND year >= 2019 AND done IS NOT NULL AND NOT (done = TRUE)",
            "      order: bm25(body, 'graph') DESC",
        ]
    );
    assert_eq!(
        lines(&db, "EXPLAIN SELECT category, COUNT(*) AS n FROM papers WHERE published >= '2026-10-01' GROUP BY category HAVING COUNT(*) >= 2 ORDER BY n DESC, category LIMIT 5"),
        [
            "Project: category, COUNT(*) AS n",
            "  Limit: 5",
            "    Sort: COUNT(*) DESC, category",
            "      Filter: COUNT(*) >= 2.0",
            "        Aggregate by category: COUNT(*)",
            "          Search papers",
            "            filter: published >= TIMESTAMP '2026-10-01T00:00:00Z'",
        ]
    );
    assert_eq!(
        lines(&db, "EXPLAIN UPDATE papers SET year = year + 1, done = TRUE WHERE category IN ('a', 'b') LIMIT 3")[..3],
        ["Update papers SET year = year + 1, done = TRUE", "  Project: id, title, body, category, tags, year, refs, rating, done, published, emb, terms, cites", "    Limit: 3"]
    );
    assert_eq!(
        lines(&db, "EXPLAIN INSERT INTO papers (id, year) VALUES ('a', 1) ON CONFLICT (id) DO UPDATE SET year = excluded.year")[0],
        "Insert into papers: 1 row, on a live key SET year = excluded.year"
    );
    assert_eq!(
        lines(&db, "EXPLAIN UPDATE papers SET rating = rating * 2 + year, title = upper(title) || '!' WHERE year > - -2")[0],
        "Update papers SET rating = (rating * 2) + year, title = upper(title) || '!'"
    );
    assert_eq!(
        lines(&db, "EXPLAIN SELECT id FROM papers WHERE REACHABLE(cites, 'p1', hops => 2) ORDER BY hops()")[2..],
        ["    filter: REACHABLE(cites, 'p1', hops => 2, min_hops => 1, dir => 'out', max_nodes => 100000)", "    order: hops()"]
    );
    // Weights keep their signs, and a TIMESTAMP decays from a time over
    // seconds, as SQL gives them.
    assert_eq!(
        lines(&db, "EXPLAIN SELECT id FROM papers ORDER BY 0.7 * bm25(body, 'graph') - 0.3 * similarity(emb, :v) DESC LIMIT 3")[3],
        "      order: 0.7 * bm25(body, 'graph') - 0.3 * similarity(emb, [0.1, 0.2, 0.3, 0.4]) DESC"
    );
    assert_eq!(
        lines(&db, "EXPLAIN SELECT id FROM papers ORDER BY decay(published, '2026-10-01', 86400, offset => 3600) DESC LIMIT 3")[3],
        "      order: decay(published, TIMESTAMP '2026-10-01T00:00:00Z', 86400, offset => 3600, decay => 0.5, shape => 'exp') DESC"
    );
    // EXPLAIN FORMAT JSON is the plan Db::explain makes, in either syntax.
    let sql = "SELECT id FROM papers WHERE year > 3 ORDER BY rating DESC LIMIT 2";
    for explain in ["EXPLAIN FORMAT JSON", "EXPLAIN (FORMAT JSON)"] {
        let json = lines(&db, &format!("{explain} {sql}"));
        assert_eq!(json.len(), 1);
        assert_eq!(Plan::from_json(&serde_json::from_str(&json[0]).unwrap()).unwrap(), db.explain(sql, &Params::new()).unwrap(), "{explain}");
    }
    // EXPLAIN does not run the statement.
    run(&db, "EXPLAIN DELETE FROM papers");
    run(&db, "INSERT INTO papers (id) VALUES ('kept')");
    run(&db, "EXPLAIN DELETE FROM papers");
    let SqlResult::Rows(r) = run(&db, "SELECT id FROM papers") else { panic!() };
    assert_eq!(r.rows.len(), 1);
    for (bad, why) in [
        ("EXPLAIN ANALYZE SELECT id FROM papers", "ANALYZE"),
        ("EXPLAIN FORMAT GRAPHVIZ SELECT id FROM papers", "GRAPHVIZ"),
        ("EXPLAIN SELECT DISTINCT category FROM papers", "compound"),
        ("EXPLAIN CREATE TABLE x (id INT PRIMARY KEY)", "SELECT, INSERT, UPDATE or DELETE"),
        ("EXPLAIN SELECT id FROM nowhere", "no such table"),
    ] {
        let e = db.sql(bad, &Params::new()).err().unwrap_or_else(|| panic!("{bad}")).to_string();
        assert!(e.contains(why), "{bad}: {e}");
    }
}

/// LIKE, CASE, arithmetic and functions are plan expressions: they go
/// through JSON and `EXPLAIN` shows them in SQL's words. The joins and
/// `WITH` results that evaluate them run over rows in memory; a Search
/// filters with what its indexes answer and says so.
#[test]
fn row_expressions_explain() {
    use strata::query::ir::{ArithOp, CmpOp, Expr, Func, When};
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"));
    let schema = db.storage().table("papers").unwrap();
    let f = |name: &str| Box::new(Expr::Field(schema.field_index(name).unwrap() as u32));
    let lit = |v: Value| Box::new(Expr::Lit(v));
    let text = |s: &str| lit(Value::Str(s.into()));
    let year2 = Expr::Arith { op: ArithOp::Mul, left: Box::new(Expr::Arith { op: ArithOp::Add, left: f("year"), right: lit(Value::Int(1)) }), right: lit(Value::Int(2)) };
    let filter = Expr::And(vec![
        Expr::Like { expr: Box::new(Expr::Call { func: Func::Lower, args: vec![*f("title")] }), pattern: text("g%"), case_insensitive: false },
        Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(Expr::Case { operand: None, whens: vec![When { when: Expr::Cmp { op: CmpOp::Gt, left: f("year"), right: lit(Value::Int(2020)) }, then: *text("new") }], otherwise: Some(text("old")) }),
            right: text("new"),
        },
        Expr::Cmp { op: CmpOp::Gt, left: Box::new(year2), right: Box::new(Expr::Neg(lit(Value::Int(4)))) },
        Expr::Not(Box::new(Expr::Like { expr: Box::new(Expr::Concat(vec![*f("category"), *text("!")])), pattern: text("%db%"), case_insensitive: true })),
    ]);
    let mut j = serde_json::json!({"version": 1, "root": {"node": "project", "input": {"node": "search", "table": "papers"}, "columns": []}});
    j["root"]["input"]["filter"] = serde_json::to_value(&filter).unwrap();
    let plan = Plan::from_json(&j).unwrap();
    assert_eq!(Plan::from_json(&plan.to_json().unwrap()).unwrap(), plan);
    let lines = plan.explain(&[&schema]);
    let line = lines.iter().find(|l| l.contains("filter:")).unwrap_or_else(|| panic!("{lines:?}"));
    assert_eq!(line.trim(), "filter: (lower(title) LIKE 'g%') AND CASE WHEN year > 2020 THEN 'new' ELSE 'old' END = 'new' AND ((year + 1) * 2) > -4 AND NOT ((category || '!') ILIKE '%db%')");
    let e = db.run(&plan).err().expect("a Search does not evaluate rows").to_string();
    assert!(e.contains("cannot filter by"), "{e}");
}
