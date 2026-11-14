//! `PACK n TOKENS` ([`chloedb::query::pack`]): a query's rows taken to
//! fill a budget of tokens, on the audit's worked example: an agent asks
//! memory how payments retry should back off, with 1,500 tokens to spend.

use chloedb::db::{Db, SqlResult};
use chloedb::query::pack::count_tokens;
use chloedb::query::Params;
use chloedb::storage::DbOptions;
use chloedb::value::Value;
use chloedb::{Field, FieldType, Schema};

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The worked example's memory, and rows of other work around it.
fn memory(dir: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.join("m.strata"), seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    run(&db, "CREATE TABLE memory (id KEYWORD PRIMARY KEY, kind KEYWORD, text TEXT, superseded BOOL, links EDGES)");
    let log: String = (0..400)
        .map(|i| match i {
            200 => "ERROR payments retry 7 failed: backoff exceeded cap after 5 s, giving up on charge ch_91. ".to_string(),
            _ => format!("INFO worker {i} heartbeat ok queue depth {} latency {} ms. ", i % 13, i % 97),
        })
        .collect();
    let rows: Vec<(&str, &str, String, bool, &str)> = vec![
        ("d1", "decision", "Decision: payments retry uses backoff v2, exponential from 100 ms, capped at 5 s, with full jitter, so a retry storm cannot pile up on the processor.".into(), false, "[]"),
        ("e2", "episode", "Episode: on Tuesday a retry storm in payments took the processor down for twenty minutes; every client retried at once with a fixed one second delay and no backoff.".into(), false, "[]"),
        ("n3", "note", "Note: payments retry uses exponential backoff capped at 5 s with jitter.".into(), false, "[]"),
        ("t4", "tool", log, false, "[]"),
        ("f5", "file", "File note for retry.rs: the payments retry loop lives in retry.rs; it reads the backoff policy from config and wraps each charge.".into(), false, "[\"d6\"]"),
        ("d6", "decision", "Decision: every charge request must carry an idempotency key, so that a repeated request is applied once.".into(), false, "[]"),
        ("s7", "decision", "Decision (old): payments retry waits a fixed one second between attempts, no backoff.".into(), true, "[]"),
        ("o8", "note", "Note: the search page caches facets for a minute.".into(), false, "[]"),
    ];
    let values: Vec<String> = rows.iter().map(|(id, kind, text, sup, links)| format!("('{id}', '{kind}', {}, {sup}, '{links}')", q(text))).collect();
    run(&db, &format!("INSERT INTO memory (id, kind, text, superseded, links) VALUES {}", values.join(", ")));
    db
}

const PACKED: &str = "SELECT id, kind, text FROM memory WHERE superseded = false ORDER BY bm25(text, :task) DESC PACK 1500 TOKENS EXCERPT 300 DEDUPE 0.8 FOLLOW links 1 HOP WITH RECEIPT";

fn task() -> Params {
    Params::new().set("task", Value::Str("payments retry backoff policy".into()))
}

fn col(cols: &[String], name: &str) -> usize {
    cols.iter().position(|c| c == name).unwrap_or_else(|| panic!("no column {name} in {cols:?}"))
}

#[test]
fn packs_the_worked_example() {
    let dir = tempfile::tempdir().unwrap();
    let db = memory(dir.path());
    let p = db.pack(PACKED, &task()).unwrap();
    let rs = &p.rs;
    let (id, text, tokens, decision, used) = (col(&rs.columns, "id"), col(&rs.columns, "text"), col(&rs.columns, "pack_tokens"), col(&rs.columns, "pack_decision"), col(&rs.columns, "pack_used"));
    assert_eq!(&rs.columns[..3], &["id", "kind", "text"]);
    let of = |k: &str| rs.rows.iter().position(|r| r[id] == Value::Str(k.into()));
    let decision_of = |k: &str| rs.rows[of(k).unwrap_or_else(|| panic!("{k} decided: {:?}", p.receipt.rows))][decision].as_str().unwrap().to_string();
    let summary: Vec<String> = rs.rows.iter().map(|r| format!("{} {:?} {}", r[id].as_str().unwrap(), r[tokens], r[decision].as_str().unwrap())).collect();

    // The superseded decision is not a candidate; the unrelated note does
    // not match.
    assert!(of("s7").is_none() && of("o8").is_none(), "{summary:?}");
    // The short note says what the decision says: one of the two is
    // skipped as the other's near-duplicate, whichever ranked lower.
    let (d1, n3) = (decision_of("d1"), decision_of("n3"));
    assert!(d1 == "skip: near-duplicate of n3" && n3 == "take" || n3 == "skip: near-duplicate of d1" && d1 == "take", "{summary:?}");
    // The log dump is cut to the lines around the error.
    assert_eq!(decision_of("t4"), "excerpt", "{summary:?}");
    let cut = rs.rows[of("t4").unwrap()][text].as_str().unwrap();
    assert!(cut.contains("backoff exceeded cap"), "{cut}");
    assert!(count_tokens(cut) <= 300, "{}", count_tokens(cut));
    // The idempotency decision matches no word of the task, and comes by
    // its link from the file note, right after it.
    assert_eq!(decision_of("d6"), "linked from f5", "{summary:?}");
    assert_eq!(of("d6").unwrap(), of("f5").unwrap() + 1, "{summary:?}");
    // The budget holds, and the receipt adds up.
    let spent: i64 = rs.rows.iter().map(|r| match r[tokens] {
        Value::Int(n) => n,
        _ => panic!(),
    }).sum();
    assert!(spent as usize <= 1500);
    assert_eq!(spent as usize, p.receipt.used);
    assert_eq!(rs.rows.last().unwrap()[used], Value::Int(spent));
    assert_eq!(p.receipt.rows.len(), rs.rows.len());
    assert_eq!(p.receipt.txn, Some(db.snapshot().txn()));
    for (r, d) in rs.rows.iter().zip(&p.receipt.rows) {
        assert_eq!(r[id], d.key);
        assert_eq!(r[decision], Value::Str(d.decision.clone()));
        assert_eq!(d.tokens == 0, d.decision.starts_with("skip"));
    }
}

#[test]
fn answers_the_rows_taken_through_sql() {
    let dir = tempfile::tempdir().unwrap();
    let db = memory(dir.path());
    let sql = PACKED.replace(" WITH RECEIPT", "");
    let SqlResult::Rows(rs) = db.sql(&sql, &task()).unwrap() else { panic!("rows") };
    assert_eq!(rs.columns, vec!["id", "kind", "text", "pack_tokens", "pack_decision"]);
    assert!(rs.rows.iter().all(|r| !r[4].as_str().unwrap().starts_with("skip")));
    assert_eq!(rs.rows.len(), rs.keys.len());
    // The same through Db::query.
    assert_eq!(db.query(&sql, &task()).unwrap().rows, rs.rows);
    // The plan says what the pack did.
    assert!(rs.stats.plan.iter().any(|l| l.starts_with("pack: ")), "{:?}", rs.stats.plan);
}

#[test]
fn a_small_budget_takes_the_best_rows_whole_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("n.strata"), seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    run(&db, "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT, pack INT)");
    let values: Vec<String> = (0..2000).map(|i| format!("({i}, 'note {i} about the retry {}', {})", "policy ".repeat(i % 5), i % 3)).collect();
    run(&db, &format!("INSERT INTO notes (id, body, pack) VALUES {}", values.join(", ")));
    let p = db.pack("SELECT id, body FROM notes ORDER BY bm25(body, 'retry policy') DESC PACK 100 TOKENS WITH RECEIPT", &Params::new()).unwrap();
    // One slice was read: its first rows filled the budget.
    assert_eq!(p.receipt.queries, 1);
    assert!(p.receipt.used <= 100 && p.receipt.used > 100 - 32, "{}", p.receipt.used);
    // The rows taken are the first of the query's order, whole.
    let top = db.query("SELECT id FROM notes ORDER BY bm25(body, 'retry policy') DESC LIMIT 64", &Params::new()).unwrap();
    let taken: Vec<&Value> = p.receipt.rows.iter().filter(|d| d.taken).map(|d| &d.key).collect();
    assert_eq!(taken, top.keys[..taken.len()].iter().collect::<Vec<_>>());
    assert!(p.receipt.rows.iter().all(|d| d.decision == "take" || d.decision == "skip: over budget"));
    // The id is a number, so it is not counted: only body is.
    let body = p.rs.columns.iter().position(|c| c == "body").unwrap();
    for (r, d) in p.rs.rows.iter().zip(&p.receipt.rows).filter(|(_, d)| d.taken) {
        assert_eq!(d.tokens, count_tokens(r[body].as_str().unwrap()));
    }
    // A column named pack is a column.
    let rs = db.query("SELECT pack FROM notes WHERE pack > 1 ORDER BY id LIMIT 2", &Params::new()).unwrap();
    assert_eq!(rs.rows, vec![vec![Value::Int(2)], vec![Value::Int(2)]]);
    // LIMIT caps the candidates read, and OFFSET starts them later.
    let p = db.pack("SELECT id, body FROM notes ORDER BY bm25(body, 'retry policy') DESC PACK 100000 TOKENS LIMIT 10 OFFSET 5", &Params::new()).unwrap();
    assert_eq!(p.receipt.rows.len(), 10);
    assert_eq!(p.receipt.rows[0].key, top.keys[5]);
}

#[test]
fn of_names_the_columns_counted_and_errors_say_what_is_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let db = memory(dir.path());
    let p = db.pack("SELECT id, kind, text FROM memory ORDER BY bm25(text, :task) DESC PACK 2000 TOKENS OF kind", &task()).unwrap();
    for (r, d) in p.rs.rows.iter().zip(&p.receipt.rows) {
        assert_eq!(d.tokens, count_tokens(r[1].as_str().unwrap()));
    }
    let e = |sql: &str| db.pack(sql, &task()).unwrap_err().to_string();
    assert!(e("SELECT id, text FROM memory PACK 100 TOKENS OF body").contains("no column body"));
    assert!(e("SELECT id FROM memory ORDER BY bm25(text, :task) DESC PACK 100 TOKENS").contains("text columns"));
    assert!(e("SELECT id, text FROM memory PACK 100 TOKENS FOLLOW links 9 HOPS").contains("at most 3"));
    assert!(e("SELECT id, text FROM memory").contains("no PACK"));
}

#[test]
fn dedupe_on_a_vector_field_compares_meanings() {
    let dir = tempfile::tempdir().unwrap();
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("v.strata"), seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    run(&db, "CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT, rank INT, emb VECTOR(3))");
    run(
        &db,
        "INSERT INTO notes (id, body, rank, emb) VALUES \
         ('a', 'retries back off exponentially up to five seconds', 3, '[1.0, 0.0, 0.0]'), \
         ('b', 'the wait between attempts doubles, at most 5 s', 2, '[0.98, 0.05, 0.0]'), \
         ('c', 'charges carry idempotency keys', 1, '[0.0, 1.0, 0.0]')",
    );
    let p = db.pack("SELECT id, body FROM notes ORDER BY rank DESC PACK 500 TOKENS DEDUPE 0.9 ON emb WITH RECEIPT", &Params::new()).unwrap();
    // The vector is read for the comparison, not answered.
    assert_eq!(p.rs.columns, vec!["id", "body", "pack_tokens", "pack_decision", "pack_score", "pack_used"]);
    let decisions: Vec<(String, String)> = p.receipt.rows.iter().map(|d| (d.key.as_str().unwrap().to_string(), d.decision.clone())).collect();
    assert_eq!(decisions, vec![("a".into(), "take".into()), ("b".into(), "skip: near-duplicate of a".into()), ("c".into(), "take".into())]);
    // The two say it in different words: without ON, both are taken.
    let p = db.pack("SELECT id, body FROM notes ORDER BY rank DESC PACK 500 TOKENS DEDUPE 0.9", &Params::new()).unwrap();
    assert_eq!(p.rs.rows.len(), 3);
}

#[test]
fn a_row_of_several_text_columns_is_not_skipped_while_the_budget_has_room() {
    let dir = tempfile::tempdir().unwrap();
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("t.strata"), seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    run(&db, "CREATE TABLE items (id INT PRIMARY KEY, title TEXT, description TEXT, rank INT)");
    // A title of a few tokens and a description of about thirty: past a
    // quarter of the budget (30 tokens of 120), not past what is left.
    let values: Vec<String> = (0..4)
        .map(|i| format!("({i}, 'trail shoe model {i}', '{}', {})", "go ".repeat(28).trim_end(), 10 - i))
        .collect();
    run(&db, &format!("INSERT INTO items (id, title, description, rank) VALUES {}", values.join(", ")));
    let p = db.pack("SELECT id, title, description FROM items ORDER BY rank DESC PACK 120 TOKENS WITH RECEIPT", &Params::new()).unwrap();
    let summary: Vec<(i64, String, usize, usize)> = p.receipt.rows.iter().map(|d| (d.key.as_i64().unwrap(), d.decision.clone(), d.tokens, d.used)).collect();
    let full = p.receipt.rows[0].full_tokens;
    assert!(full > 120 / 4 && full < 60, "{summary:?}");
    // Each row is taken whole, or excerpted, while an excerpt fits.
    for d in &p.receipt.rows {
        let left_before = 120 - (d.used - d.tokens);
        if left_before >= full {
            assert!(d.taken, "{summary:?}");
        }
        assert!(d.decision != "skip: over budget" || left_before < 32 + 8, "{summary:?}");
    }
    assert!(p.receipt.used <= 120);
    assert!(p.receipt.rows.iter().filter(|d| d.taken).count() >= 2, "{summary:?}");
}

fn empty_db(dir: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    Db::create(&dir.join("p.strata"), seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap()
}

/// A fused ranking is read deeper each time, from its first row, not page
/// by page: the second page of `rrf` at a depth of its page's size comes
/// back short when one input matches few rows, which the pack used to read
/// as the end of the query, and stopped with most of the budget unspent
/// (N-B7, P2).
#[test]
fn a_fused_ranking_fills_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let db = empty_db(dir.path());
    run(&db, "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)");
    // 3,000 notes of about 20 tokens; 40 say "retry".
    let values: Vec<String> = (0..3000)
        .map(|i| {
            let word = if i % 75 == 0 { "retry" } else { "other" };
            format!("({i}, 'note {i} on the {word} work of team {} with words to make it longer than a few tokens')", i % 17)
        })
        .collect();
    run(&db, &format!("INSERT INTO notes (id, body) VALUES {}", values.join(", ")));
    let sql = "SELECT id, body FROM notes ORDER BY rrf(bm25(body, 'retry'), bm25(body, 'note')) DESC PACK 8000 TOKENS WITH RECEIPT";
    let p = db.pack(sql, &Params::new()).unwrap();
    assert!(p.receipt.used > 8000 - 32, "{} of 8000 tokens in {} rows, by {} queries", p.receipt.used, p.receipt.rows.len(), p.receipt.queries);
    // No row is decided twice, and the 40 that match both are among them.
    let mut keys: Vec<&Value> = p.receipt.rows.iter().map(|d| &d.key).collect();
    let n = keys.len();
    let int = |v: &Value| match v {
        Value::Int(i) => *i,
        v => panic!("{v:?}"),
    };
    let retry = p.receipt.rows.iter().filter(|d| int(&d.key) % 75 == 0).count();
    assert_eq!(retry, 40);
    keys.sort_by_key(|k| int(k));
    keys.dedup();
    assert_eq!(keys.len(), n);
}

/// Text without spaces is cut inside its runs, so a row never spends more
/// than the budget (P1, N-C3); its excerpt is around the query's words as
/// the field's analyzer finds them.
#[test]
fn text_without_spaces_keeps_to_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let db = empty_db(dir.path());
    run(&db, "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT OPTIONS(analyzer = 'standard'))");
    let filler = "これは関係のない長い文章です。".repeat(3000);
    let page = format!("{filler}東京都の天気は晴れです。{filler}");
    run(&db, &format!("INSERT INTO docs (id, body) VALUES (1, {}), (2, {})", q(&page), q(&"中文没有空格".repeat(5000))));
    for budget in [40, 100, 1000] {
        let p = db.pack(&format!("SELECT id, body FROM docs ORDER BY id PACK {budget} TOKENS WITH RECEIPT"), &Params::new()).unwrap();
        assert!(p.receipt.used <= budget, "{} of {budget}", p.receipt.used);
        for (r, d) in p.rs.rows.iter().zip(&p.receipt.rows).filter(|(_, d)| d.taken) {
            assert_eq!(d.decision, "excerpt");
            assert!(count_tokens(r[1].as_str().unwrap()) == d.tokens && d.tokens <= budget);
        }
    }
    let p = db.pack("SELECT id, body FROM docs WHERE MATCH(body, '天気') ORDER BY bm25(body, '天気') DESC PACK 200 TOKENS", &Params::new()).unwrap();
    let cut = p.rs.rows[0][1].as_str().unwrap();
    assert!(cut.contains("天気") && count_tokens(cut) <= 200, "{cut}");
}

/// `PACK :n TOKENS`: the budget a parameter, as LIMIT may be.
#[test]
fn the_budget_may_be_a_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let db = memory(dir.path());
    let sql = "SELECT id, text FROM memory ORDER BY bm25(text, :task) DESC PACK :budget TOKENS WITH RECEIPT";
    let p = db.pack(sql, &task().set("budget", Value::Int(200))).unwrap();
    assert_eq!(p.receipt.budget, 200);
    assert!(p.receipt.used <= 200);
    assert!(db.pack(sql, &task()).unwrap_err().to_string().contains("no parameter budget"));
    let SqlResult::Rows(rs) = db.sql(sql, &task().set("budget", Value::Int(300))).unwrap() else { panic!("rows") };
    assert!(rs.columns.contains(&"pack_tokens".to_string()));
}

/// A row with no text spends nothing and says so; `WITH RECEIPT` answers a
/// row skipped without its text, so the rows' text fits the budget (P5,
/// P6); a text column that is NULL in the first read and text later is
/// counted (P8).
#[test]
fn rows_without_text_and_rows_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let db = empty_db(dir.path());
    run(&db, "CREATE TABLE notes (id INT PRIMARY KEY, rank INT, title KEYWORD, extra TEXT)");
    let values: Vec<String> = (0..300)
        .map(|i| match i {
            0..=9 => format!("({i}, {}, NULL, NULL)", 1000 - i),
            10..=99 => format!("({i}, {}, 't{i}', NULL)", 1000 - i),
            _ => format!("({i}, {}, 't{i}', 'an extra note of {i} with a few more words in it')", 1000 - i),
        })
        .collect();
    run(&db, &format!("INSERT INTO notes (id, rank, title, extra) VALUES {}", values.join(", ")));
    let p = db.pack("SELECT id, title, extra FROM notes ORDER BY rank DESC PACK 1500 TOKENS WITH RECEIPT", &Params::new()).unwrap();
    for d in &p.receipt.rows[..10] {
        assert_eq!((d.decision.as_str(), d.tokens), ("skip: no text", 0));
    }
    let (title, extra) = (1, 2);
    for (r, d) in p.rs.rows.iter().zip(&p.receipt.rows) {
        let text = |j: usize| r[j].as_str().map_or(0, count_tokens);
        match d.taken {
            true => assert_eq!(d.tokens, text(title) + text(extra), "{r:?}"),
            false => assert!(r[title] == Value::Null && r[extra] == Value::Null, "{r:?}"),
        }
    }
    let late = p.receipt.rows.iter().position(|d| d.key == Value::Int(120)).expect("row 120 decided");
    assert!(p.receipt.rows[late].full_tokens > count_tokens("t120"), "{:?}", p.receipt.rows[late]);
    assert!(p.receipt.used <= 1500 && p.receipt.used > 1500 - 32, "{}", p.receipt.used);
    // The answer's text adds up to what the pack spent.
    let spent: usize = p.rs.rows.iter().map(|r| r[title].as_str().map_or(0, count_tokens) + r[extra].as_str().map_or(0, count_tokens)).sum();
    assert_eq!(spent, p.receipt.used);
}

/// `LIMIT 0` answers no rows with the pack's columns; a query that reads
/// back otherwise once printed (`- -1`) runs, as nothing is printed
/// (N-B8, N-B9).
#[test]
fn limit_zero_and_queries_that_do_not_print_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = memory(dir.path());
    let rs = db.query("SELECT id, text FROM memory ORDER BY bm25(text, :task) DESC PACK 100 TOKENS LIMIT 0", &task()).unwrap();
    assert_eq!(rs.columns, vec!["id", "text", "pack_tokens", "pack_decision"]);
    assert!(rs.rows.is_empty());
    run(&db, "CREATE TABLE ranked (id INT PRIMARY KEY, rank INT, body TEXT)");
    run(&db, "INSERT INTO ranked (id, rank, body) VALUES (1, 0, 'zero'), (2, 1, 'one'), (3, 2, 'two')");
    let p = db.pack("SELECT id, body FROM ranked WHERE rank > - -1 ORDER BY rank DESC PACK 300 TOKENS", &Params::new()).unwrap();
    assert_eq!(p.rs.rows.len(), 1);
}

/// A binding's Arrow answer of a PACK query is its rows (N-B9).
#[cfg(feature = "datafusion")]
#[test]
fn query_leaf_runs_pack() {
    let dir = tempfile::tempdir().unwrap();
    let db = memory(dir.path());
    let sql = PACKED.replace(" WITH RECEIPT", "");
    match db.query_leaf(&sql, &task()).unwrap() {
        chloedb::query::exec::Leaf::Rows(rs) => assert_eq!(rs.rows, db.query(&sql, &task()).unwrap().rows),
        _ => panic!("rows"),
    }
}

/// Rows of a title and a description need room for both and an excerpt:
/// once what is left is less, every row is turned away, and the pack stops
/// reading rather than ranking up to 1,000 rows only to skip them (PRF-11:
/// PACK 500 TOKENS over rrf had a p95 eight times its p50).
#[test]
fn a_budget_too_small_for_the_rows_left_stops_reading() {
    let dir = tempfile::tempdir().unwrap();
    let db = empty_db(dir.path());
    run(&db, "CREATE TABLE items (id INT PRIMARY KEY, title TEXT, description TEXT)");
    let title = "quiet leather camera for travel, with a strap and a case of waxed canvas";
    let description = "a quiet camera in leather for travel and the city, ".repeat(4);
    let description = description.trim_end();
    let values: Vec<String> = (0..3000).map(|i| format!("({i}, '{title}', '{description}')")).collect();
    run(&db, &format!("INSERT INTO items (id, title, description) VALUES {}", values.join(", ")));
    // Eight rows whole, then a little more than an excerpt's 32 tokens
    // but less than the title and an excerpt: no row fits any more.
    let (rest, full) = (count_tokens(title), count_tokens(title) + count_tokens(description));
    let budget = 8 * full + 32 + rest / 2;
    assert!(rest >= 4 && full > 32 + rest, "{rest} {full}");
    let sql = format!("SELECT id, title, description FROM items ORDER BY rrf(bm25(description, 'quiet camera'), bm25(title, 'leather camera')) DESC PACK {budget} TOKENS WITH RECEIPT");
    let p = db.pack(&sql, &Params::new()).unwrap();
    let summary = format!("{} of {budget} tokens, {} rows decided, {} queries", p.receipt.used, p.receipt.rows.len(), p.receipt.queries);
    assert_eq!(p.receipt.rows.iter().filter(|d| d.taken).count(), 8, "{summary}");
    assert_eq!(budget - p.receipt.used, 32 + rest / 2, "{summary}");
    // The first read's rows were enough to see that: no deeper read.
    assert_eq!(p.receipt.queries, 1, "{summary}");
    assert_eq!(p.receipt.rows.len(), 64, "{summary}");
    assert!(p.receipt.rows[8..].iter().all(|d| d.decision == "skip: over budget"), "{summary}");
}
