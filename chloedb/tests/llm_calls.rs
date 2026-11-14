//! A language model's answers as a query's values (`llm()`,
//! [`chloedb::query::llm`]): asked on the rows a query returns, through a
//! stored function or written in the query, inside other expressions, as
//! a choice, a number or text; ranking the top rows of a first ranking in
//! ORDER BY; each question asked once and kept in memory; the limits of
//! rows and requests; and what is refused. Against a mock of Anthropic's
//! API on loopback.

#![cfg(feature = "hosted")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as J};
use chloedb::db::{Db, SqlResult};
use chloedb::query::exec::ResultSet;
use chloedb::query::llm::{self, Limits};
use chloedb::query::Params;
use chloedb::storage::DbOptions;
use chloedb::value::Value;

/// A server on loopback that answers as a model would: a choice by the
/// words of the text (great: positive, awful: negative, else mixed), a
/// number (how many times the text says "kettle"), or, for text, its first
/// three words upper-cased after "ABOUT:". Each request's body is kept.
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<J>>>,
}

impl Mock {
    fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<J>>> = Arc::default();
        let s = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let s = s.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    let mut len = 0usize;
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).unwrap();
                        let h = h.trim_end().to_ascii_lowercase();
                        if h.is_empty() {
                            break;
                        }
                        if let Some(v) = h.strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; len];
                    r.read_exact(&mut body).unwrap();
                    let body: J = serde_json::from_slice(&body).unwrap();
                    let system = body["system"].as_str().unwrap_or("").to_string();
                    let user = body["messages"][0]["content"].as_str().unwrap().to_string();
                    let made = if system.contains("Answer with one of these") {
                        if user.contains("great") {
                            "Positive.".to_string()
                        } else if user.contains("awful") {
                            "it is negative".to_string()
                        } else {
                            "MIXED".to_string()
                        }
                    } else if system.contains("Answer with a number") {
                        format!("Grade: {}", user.matches("kettle").count())
                    } else {
                        format!("ABOUT: {}", user.split_whitespace().take(3).collect::<Vec<_>>().join(" ").to_uppercase())
                    };
                    s.lock().unwrap().push(body);
                    let text = json!({"type": "message", "content": [{"type": "text", "text": made}]}).to_string();
                    let mut w = conn;
                    let _ = w.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len()).as_bytes());
                });
            }
        });
        Mock { url, seen }
    }

    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn sql(db: &Db, text: &str) -> SqlResult {
    db.sql(text, &Params::new()).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn query(db: &Db, text: &str) -> ResultSet {
    db.query(text, &Params::new()).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn refused(db: &Db, text: &str, says: &str) {
    let e = db.sql(text, &Params::new()).err().unwrap_or_else(|| panic!("{text} ran")).to_string();
    assert!(e.contains(says), "{text}: {e}");
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

const MODEL: &str = "anthropic/claude-haiku-5-5";

fn reviews(dir: &std::path::Path) -> Db {
    let db = Db::create_empty(&dir.join("r.strata"), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    let _ = db.open_embedders(&dir.join("models"));
    sql(&db, "CREATE TABLE reviews (id KEYWORD PRIMARY KEY, body TEXT, rating INT)");
    for (id, body, rating) in [
        ("a", "a great kettle that boils fast, kettle of the year", 5),
        ("b", "awful kettle, it leaks", 1),
        ("c", "the kettle is fine and the kettle lid is fine and kettle", 3),
        ("d", "short", 4),
        ("e", "a toaster review with no kettle words at all", 2),
    ] {
        sql(&db, &format!("INSERT INTO reviews (id, body, rating) VALUES ('{id}', '{body}', {rating})"));
    }
    db
}

#[test]
fn a_model_answers_about_the_rows_a_query_returns() {
    let mock = Mock::start();
    std::env::set_var("ANTHROPIC_BASE_URL", &mock.url);
    std::env::set_var("ANTHROPIC_API_KEY", "ak-llm");
    let dir = tempfile::tempdir().unwrap();
    let db = reviews(dir.path());

    // Through a stored function, on the rows after ORDER BY and LIMIT: two
    // requests, and the column keeps the call's name.
    sql(&db, &format!("CREATE FUNCTION about(t TEXT) RETURNS TEXT AS llm('{MODEL}', 'Say what the review is about.', :t)"));
    let q = "SELECT id, about(body) FROM reviews WHERE MATCH(body, 'kettle') ORDER BY bm25(body, 'kettle') DESC LIMIT 2";
    let rs = query(&db, q);
    assert_eq!(rs.columns, vec!["id", "about(body)"]);
    assert_eq!(rs.rows.len(), 2);
    for r in &rs.rows {
        let Value::Str(about) = &r[1] else { panic!("{r:?}") };
        assert!(about.starts_with("ABOUT: "), "{about}");
    }
    assert_eq!(mock.requests(), 2);
    {
        let seen = mock.seen.lock().unwrap();
        let b = &seen[0];
        assert_eq!((b["model"].as_str(), b["system"].as_str(), b["max_tokens"].as_u64()), (Some("claude-haiku-5-5"), Some("Say what the review is about."), Some(1024)));
        assert!(b.get("temperature").is_none(), "current Claude models refuse a temperature: {b}");
    }
    assert!(rs.stats.plan.iter().any(|l| l.starts_with(&format!("llm: {MODEL} asked about 2 rows: 2 requests"))), "{:?}", rs.stats.plan);
    // The same query again asks nothing: the answers are kept.
    let again = query(&db, q);
    assert_eq!((again.rows, mock.requests()), (rs.rows.clone(), 2));
    assert!(llm::usage().cached >= 2);

    // A choice, any case, read from an answer that says more; inside an
    // expression; a parameter's instructions; rows of no input ask nothing.
    sql(&db, &format!("CREATE FUNCTION mood(t TEXT) RETURNS KEYWORD AS llm('{MODEL}', 'How does the review feel?', :t, choices => 'positive,negative,mixed')"));
    let rs = query(&db, "SELECT id, mood(body) AS mood, upper(mood(body)) AS loud FROM reviews WHERE id IN ('a', 'b', 'c') LIMIT 5");
    let mut got: Vec<(Value, Value, Value)> = rs.rows.iter().map(|r| (r[0].clone(), r[1].clone(), r[2].clone())).collect();
    got.sort_by_key(|(id, _, _)| id.as_str().unwrap().to_string());
    assert_eq!(got, vec![(s("a"), s("positive"), s("POSITIVE")), (s("b"), s("negative"), s("NEGATIVE")), (s("c"), s("mixed"), s("MIXED"))]);
    let n = mock.requests();
    assert_eq!(n, 5, "one question a row, the same for both columns");
    let system = mock.seen.lock().unwrap()[n - 1]["system"].as_str().unwrap().to_string();
    assert_eq!(system, "How does the review feel?\n\nAnswer with one of these, and nothing else: positive, negative, mixed.");
    let p = Params::new().set("how", s("Say what it is about."));
    let rs = db.query(&format!("SELECT id, CASE WHEN length(body) < 10 THEN body ELSE llm('{MODEL}', :how, body) END AS s FROM reviews WHERE id IN ('d', 'e') LIMIT 5"), &p).unwrap();
    let mut got: Vec<(Value, Value)> = rs.rows.iter().map(|r| (r[0].clone(), r[1].clone())).collect();
    got.sort_by_key(|(id, _)| id.as_str().unwrap().to_string());
    assert_eq!(got, vec![(s("d"), s("short")), (s("e"), s("ABOUT: A TOASTER REVIEW"))]);
    assert_eq!(mock.requests(), 6, "the short review is not asked about: a CASE asks for the branch it takes");
    // coalesce asks only where the values before the call are NULL.
    let rs = query(&db, &format!("SELECT id, coalesce(CASE WHEN rating >= 4 THEN 'liked' END, llm('{MODEL}', 'Why the low rating?', body)) AS why FROM reviews WHERE id IN ('a', 'b') LIMIT 5"));
    let mut got: Vec<(Value, Value)> = rs.rows.iter().map(|r| (r[0].clone(), r[1].clone())).collect();
    got.sort_by_key(|(id, _)| id.as_str().unwrap().to_string());
    assert_eq!(got, vec![(s("a"), s("liked")), (s("b"), s("ABOUT: AWFUL KETTLE, IT"))]);
    assert_eq!(mock.requests(), 7);

    // In ORDER BY: the first ranking's top rows ranked by the answers,
    // ties in the first ranking's order, score() the answer.
    let rs = query(
        &db,
        &format!("WITH top AS (SELECT * FROM reviews WHERE MATCH(body, 'kettle') ORDER BY bm25(body, 'kettle') DESC LIMIT 4) SELECT id, rating, score() FROM top ORDER BY llm('{MODEL}', 'Grade how much it is about kettles.', body, returns => 'number') DESC LIMIT 2"),
    );
    assert_eq!(rs.columns, vec!["id", "rating", "score()"]);
    assert_eq!(rs.rows, vec![vec![s("c"), Value::Int(3), Value::Float(3.0)], vec![s("a"), Value::Int(5), Value::Float(2.0)]]);
    assert_eq!(rs.scores, vec![3.0, 2.0]);
    let rs = query(&db, &format!("SELECT id FROM reviews WHERE id IN ('a', 'b', 'c') ORDER BY llm('{MODEL}', 'Grade how much it is about kettles.', body, returns => 'number') ASC, rating DESC LIMIT 5 OFFSET 1"));
    assert_eq!(rs.rows, vec![vec![s("a")], vec![s("c")]], "b and a grade 1 and 2, c 3; OFFSET 1 skips b");

    // A query of more rows than a query may ask about, and past the
    // process's requests a minute, fails before asking.
    llm::set_limits(Limits { rows: 3, per_minute: None });
    let e = db.query(&format!("SELECT id, llm('{MODEL}', 'Say it', body) FROM reviews LIMIT 5"), &Params::new()).unwrap_err().to_string();
    assert!(e.contains("calls a model for 3 at most"), "{e}");
    let e = db.query(&format!("SELECT id FROM reviews ORDER BY llm('{MODEL}', 'Grade it', body, returns => 'number') DESC LIMIT 1"), &Params::new()).unwrap_err().to_string();
    assert!(e.contains("ranks 3 rows at most"), "{e}");
    llm::set_limits(Limits { rows: 100, per_minute: Some(1) });
    let n = mock.requests();
    let e = db.query(&format!("SELECT id, llm('{MODEL}', 'Name its color', body) FROM reviews LIMIT 5"), &Params::new()).unwrap_err().to_string();
    assert!(e.contains("a minute are left"), "{e}");
    assert_eq!(mock.requests(), n);
    llm::set_limits(Limits::default());

    // EXPLAIN shows the plan of the rows it asks about.
    let plan = db.explain(q, &Params::new()).unwrap();
    assert!(!plan.explain(&[]).is_empty());

    // Refused: a call where it would run for rows a query does not return,
    // and a function whose call is wrong, when it is made.
    for bad in [
        format!("SELECT id FROM reviews WHERE llm('{MODEL}', 'Is it spam?', body, returns => 'bool') LIMIT 5"),
        format!("SELECT rating, count(*) FROM reviews GROUP BY rating HAVING llm('{MODEL}', 'x', rating) = 'yes'"),
        format!("WITH t AS (SELECT id, llm('{MODEL}', 'x', body) AS a FROM reviews LIMIT 3) SELECT * FROM t"),
    ] {
        let e = db.query(&bad, &Params::new()).unwrap_err().to_string();
        assert!(e.contains("not part of WHERE"), "{bad}: {e}");
    }
    refused(&db, &format!("CREATE FUNCTION f(t TEXT) RETURNS TEXT AS llm('{MODEL}', 'x', :t, temperature => 1)"), "no option temperature");
    refused(&db, "CREATE FUNCTION f(t TEXT, m TEXT) RETURNS TEXT AS llm(:m, 'x', :t)", "the model is a string");
    refused(&db, &format!("CREATE FUNCTION f(q TEXT) RETURNS TABLE AS SELECT id, llm('{MODEL}', 'x', body) FROM reviews LIMIT 3"), "RETURNS TABLE");
    refused(&db, "CREATE FUNCTION llm(t TEXT) RETURNS TEXT AS :t", "llm");
}

/// A query names its model; it never reads an `endpoint.json` from outside
/// the models directory, and an endpoint's `key_env` names a model's key
/// variable, never another of the process's.
#[test]
fn a_query_names_a_model_never_a_path() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let db = reviews(dir.path());
    let endpoint = |var: &str| json!({"api": "anthropic", "url": format!("{}/v1/messages", mock.url), "model": "m", "key_env": var}).to_string();
    std::env::set_var("CHLOEDB_LLM_TEST_NOT_A_KEY", "not-for-a-model");
    std::env::set_var("CHLOEDB_MODEL_KEY_LLM_TEST", "for-a-model");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("endpoint.json"), endpoint("CHLOEDB_MODEL_KEY_LLM_TEST")).unwrap();
    for model in [outside.display().to_string(), "../outside".into(), "a/../../outside".into(), "./outside".into(), "a/b/c".into(), "a\\b".into()] {
        let e = db.query(&format!("SELECT llm('{model}', 'say ok', body) FROM reviews LIMIT 1"), &Params::new()).unwrap_err().to_string();
        assert!(e.contains("not a path") || e.contains("is a provider's model"), "{model}: {e}");
    }
    assert_eq!(mock.requests(), 0, "no endpoint outside the models directory was asked");

    // Inside it, an endpoint's key variable is a model's.
    for (name, var) in [("other-var", "CHLOEDB_LLM_TEST_NOT_A_KEY"), ("own-key", "CHLOEDB_MODEL_KEY_LLM_TEST")] {
        std::fs::create_dir_all(dir.path().join("models").join(name)).unwrap();
        std::fs::write(dir.path().join("models").join(name).join("endpoint.json"), endpoint(var)).unwrap();
    }
    let e = db.query("SELECT llm('other-var', 'say ok', body) FROM reviews LIMIT 1", &Params::new()).unwrap_err().to_string();
    assert!(e.contains("key_env") && e.contains("CHLOEDB_MODEL_KEY_"), "{e}");
    assert_eq!(mock.requests(), 0);
    let rs = query(&db, "SELECT llm('own-key', 'say ok', body) FROM reviews WHERE id = 'a' LIMIT 1");
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(mock.requests(), 1);
}
