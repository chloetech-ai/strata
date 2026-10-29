//! A compound query's steps (each side of a join, a subquery, a `WITH`)
//! bind from syntax trees built for them, where they were printed as SQL
//! and parsed again: `- -1` printed as `--1`, which starts a comment, and
//! a column named like a keyword (`order`) or with a space did not parse.
//! Each step's SQL, which a coordinator sends its shards, parses back to
//! the step's plan.

use strata::db::Db;
use strata::graph::{edges_value, Edge};
use strata::query::compound;
use strata::query::exec::ExecOptions;
use strata::query::ir::Plan;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema};

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("order", FieldType::Int),
            Field::new("limit", FieldType::Keyword),
            Field::new("body", FieldType::Text { positions: false }),
            Field::new("links", FieldType::Edges),
            Field::new("n", FieldType::Int),
            Field::new("doc no", FieldType::Int),
        ],
        Some("id"),
    )
    .unwrap()
}

fn open(dir: &std::path::Path) -> Db {
    let db = Db::create(&dir.join("t.strata"), schema(), DbOptions::default()).unwrap();
    let row = |id: &str, order: i64, limit: &str, body: &str, links: &[&str], n: i64| {
        let es: Vec<Edge> = links.iter().map(|t| Edge::to(*t)).collect();
        vec![Value::Str(id.into()), Value::Int(order), Value::Str(limit.into()), Value::Str(body.into()), edges_value(&es), Value::Int(n), Value::Int(order * 10)]
    };
    db.load([row("a", 1, "x", "red fox", &["b"], 1), row("b", 2, "y", "blue fox", &["c"], -2), row("c", 3, "x", "red hen", &[], 3)]).unwrap();
    db
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

/// The query's rows, run with a runner that checks that each step's SQL
/// binds to the step's plan, against the same from `Db::query`.
fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    let schema = schema();
    let snap = db.snapshot();
    let mut texts = Vec::new();
    let got = compound::run(sql, &schema, &Params::new(), &mut |step| {
        let again = Plan::bind(step.sql, &schema, step.params).unwrap_or_else(|e| panic!("{}: {e}", step.sql));
        assert_eq!(&again, step.plan, "{}", step.sql);
        texts.push(step.sql.to_string());
        db.run_at(&snap, step.plan, &ExecOptions::default())
    })
    .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(texts.iter().all(|t| !t.contains("--")), "{texts:?}");
    let want = db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(got.rows, want.rows, "{sql}");
    got.rows
}

#[test]
fn steps_bind_their_own_syntax_trees() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let cases: Vec<(&str, Vec<Vec<Value>>)> = vec![
        // A join's sides, with `- -1` in each.
        ("SELECT a.id, b.id FROM t a JOIN t b ON a.links = b.id WHERE a.n >= - -1 AND b.n < - -1", vec![vec![s("a"), s("b")]]),
        // Join keys and conditions on columns named like keywords.
        ("SELECT a.id, b.id FROM t a JOIN t b ON a.\"limit\" = b.\"limit\" WHERE a.\"order\" < b.\"order\"", vec![vec![s("a"), s("c")]]),
        // The rows linking to the left side's (an EDGES field).
        ("SELECT a.id, b.\"order\" FROM t a JOIN t b ON b.links = a.id WHERE a.\"order\" >= - -2 ORDER BY a.\"order\"", vec![vec![s("b"), Value::Int(1)], vec![s("c"), Value::Int(2)]]),
        // A column whose name has a space.
        ("SELECT a.id, b.\"doc no\" FROM t a JOIN t b ON a.links = b.id WHERE a.\"doc no\" > - -15", vec![vec![s("b"), Value::Int(30)]]),
        // A scan ranked by a score.
        ("SELECT a.id, b.id FROM t a JOIN t b ON a.links = b.id ORDER BY bm25(a.body, 'red') DESC", vec![vec![s("a"), s("b")]]),
        // Subqueries.
        ("SELECT id FROM t WHERE \"order\" IN (SELECT \"order\" FROM t WHERE n > - -1)", vec![vec![s("c")]]),
        ("SELECT id FROM t WHERE \"order\" = (SELECT \"order\" FROM t WHERE n = - -3)", vec![vec![s("c")]]),
        ("SELECT id FROM t WHERE EXISTS (SELECT id FROM t WHERE \"order\" = - -3) AND n > (-1) ORDER BY \"order\"", vec![vec![s("a")], vec![s("c")]]),
        // WITH rows of the table, in their order.
        ("WITH w AS (SELECT id FROM t WHERE \"limit\" = 'x' ORDER BY \"order\" DESC) SELECT id, \"order\" FROM w WHERE n >= - -1", vec![vec![s("c"), Value::Int(3)], vec![s("a"), Value::Int(1)]]),
        // DISTINCT, and a derived table.
        ("SELECT DISTINCT \"limit\" FROM t WHERE n >= - -1", vec![vec![s("x")]]),
        ("SELECT d.id FROM (SELECT id, \"order\" FROM t WHERE n > - -1) AS d JOIN t b ON d.id = b.id", vec![vec![s("c")]]),
    ];
    for (sql, want) in cases {
        assert_eq!(rows(&db, sql), want, "{sql}");
    }
    // What the query's plan shows of a step is SQL that parses back.
    let plan = db.query("SELECT a.id FROM t a JOIN t b ON a.links = b.id WHERE a.n >= - -1", &Params::new()).unwrap().stats.plan;
    assert!(plan.iter().any(|l| l.starts_with("scan a: 2 rows: ") && l.contains("n >= -(-1)")), "{plan:?}");
}
