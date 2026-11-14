//! The relevance autopilot: logged queries that rank by a rank profile,
//! graded, replayed on one snapshot with the profile and with the numbers
//! its search finds; a proposal kept with its proof, applied only while the
//! profile is the one it was measured against.
//!
//! The corpus is made so that the answer is known: a query is one word, the
//! rows with the word in their title are the ones it wants (grade 3), those
//! with it only in their body are related (grade 1), and the profile ranks
//! by the body with a small weight on the title. Raising that weight is
//! the better ranking.

use serde_json::json;
use chloedb::autopilot::{self, Options, Verdict};
use chloedb::db::{Db, SqlResult};
use chloedb::query::Params;
use chloedb::rank_log::Log;
use chloedb::storage::DbOptions;
use chloedb::value::Value;

const WORDS: usize = 160;
const DOCS: usize = 900;
const QUERIES: usize = 90;
const SQL: &str = "SELECT id FROM docs ORDER BY rank_profile('p', q => :q) LIMIT 10";

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn word(i: usize) -> String {
    format!("w{i:03}")
}

fn sql(db: &Db, text: &str) -> SqlResult {
    db.sql(text, &Params::new()).unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// The database, its documents' titles and bodies, as words.
fn corpus(path: &std::path::Path) -> (Db, Vec<(Vec<usize>, Vec<usize>)>) {
    let db = Db::create_empty(path, DbOptions::default()).unwrap();
    sql(&db, "CREATE TABLE docs (id KEYWORD PRIMARY KEY, title TEXT, body TEXT)");
    let mut rng = Rng(7);
    let mut docs = Vec::new();
    let mut values = Vec::new();
    for i in 0..DOCS {
        let title: Vec<usize> = (0..2).map(|_| rng.below(WORDS)).collect();
        let body: Vec<usize> = (0..30).map(|_| rng.below(WORDS)).collect();
        let text = |ws: &[usize]| ws.iter().map(|w| word(*w)).collect::<Vec<_>>().join(" ");
        values.push(format!("('d{i}', '{}', '{}')", text(&title), text(&body)));
        docs.push((title, body));
    }
    sql(&db, &format!("INSERT INTO docs VALUES {}", values.join(", ")));
    sql(&db, "CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY bm25(body, :q) + 0.05 * bm25(title, :q) DESC");
    (db, docs)
}

fn grades(docs: &[(Vec<usize>, Vec<usize>)], w: usize) -> Vec<(String, f64)> {
    docs.iter()
        .enumerate()
        .filter_map(|(i, (t, b))| {
            let g = if t.contains(&w) { 3.0 } else if b.contains(&w) { 1.0 } else { return None };
            Some((format!("d{i}"), g))
        })
        .collect()
}

/// Logs each query (some twice) and grades its rows in the judgments table.
fn log_and_grade(db: &Db, docs: &[(Vec<usize>, Vec<usize>)]) {
    let mut judgments = Vec::new();
    for w in 0..QUERIES {
        let params = Params::new().set("q", Value::Str(word(w)));
        for _ in 0..(1 + (w % 3 == 0) as usize) {
            db.query_logged(SQL, &params, &Log::default()).unwrap();
        }
        let qid = autopilot::qid_of(SQL, &params);
        for (key, g) in grades(docs, w) {
            judgments.push(autopilot::judgment_doc(&qid, &word(w), &key, g, "human", 0));
        }
    }
    db.add_judgments(autopilot::JUDGMENTS, &judgments).unwrap();
}

#[test]
fn the_log_keeps_each_requests_query_on_its_first_row() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = corpus(&dir.path().join("db"));
    let params = Params::new().set("q", Value::Str(word(3)));
    let r = db.query_logged(SQL, &params, &Log::default()).unwrap();
    let rs = db.query("SELECT position, query FROM rank_log WHERE request = :r ORDER BY position ASC", &Params::new().set("r", Value::Str(r.request))).unwrap();
    assert!(rs.rows.len() > 1, "{:?}", rs.rows);
    let Value::Str(q) = &rs.rows[0][1] else { panic!("no query on the first row: {:?}", rs.rows[0]) };
    let q: serde_json::Value = serde_json::from_str(q).unwrap();
    assert_eq!(q, json!({"sql": SQL, "params": {"q": "w003"}}));
    assert!(rs.rows[1..].iter().all(|r| r[1] == Value::Null), "{:?}", rs.rows);
}

#[test]
fn it_proposes_the_better_weight_with_proof_and_applies_it() {
    let dir = tempfile::tempdir().unwrap();
    let (db, docs) = corpus(&dir.path().join("db"));
    log_and_grade(&db, &docs);
    let before = db.snapshot().table("docs").unwrap().rank_profile("p").unwrap().clone();
    let r = db.autopilot(&Options::default()).unwrap();
    let j = r.to_json();
    assert_eq!(r.verdict, Verdict::Propose, "{}", serde_json::to_string_pretty(&j).unwrap());
    assert_eq!(r.table.as_deref(), Some("docs"));
    assert_eq!(r.queries, QUERIES);
    assert_eq!(r.requests, (QUERIES + QUERIES.div_ceil(3)) as u64);
    assert_eq!(r.judged, QUERIES);
    assert!(r.held_out.queries > 10 && r.train.queries > 40, "{j}");
    assert!(r.held_out.after > r.held_out.before + 0.02, "{j}");
    assert!(r.held_out.better > r.held_out.worse, "{j}");
    assert!(r.held_out.ci.0 > 0.0, "{j}");
    // The one number moved is the title's weight, upwards.
    assert_eq!(r.changes.len(), 1, "{j}");
    assert!(r.changes[0].1 > 0.05, "{j}");
    assert!(r.changes[0].0.knob.to_string().contains("bm25(title"), "{j}");
    let statement = r.statement.clone().unwrap();
    assert!(statement.starts_with("CREATE OR REPLACE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY bm25(body, :q) + "), "{statement}");
    // Replaying is read-only: the profile is as it was, and the same data
    // gives the same report.
    assert_eq!(db.snapshot().table("docs").unwrap().rank_profile("p"), Some(&before));
    let again = db.autopilot(&Options::default()).unwrap();
    assert_eq!(again.to_json()["held_out"], j["held_out"]);
    assert_eq!(again.statement, r.statement);
    // Kept, then applied: queries rank by the new numbers.
    let id = db.keep_proposal(&r).unwrap();
    let kept = db.proposal(&id, true).unwrap();
    assert_eq!(kept["status"], "open");
    assert_eq!(kept["verdict"], "propose");
    let proof: serde_json::Value = serde_json::from_str(kept["proof"].as_str().unwrap()).unwrap();
    assert_eq!(proof["held_out"], j["held_out"]);
    db.apply_proposal(&id, false).unwrap();
    let now = db.snapshot().table("docs").unwrap().rank_profile("p").unwrap().clone();
    assert_eq!(Some(now.hash()), r.candidate.as_ref().map(|c| c.hash()));
    assert_eq!(db.proposal(&id, false).unwrap()["status"], "applied");
    assert!(db.apply_proposal(&id, true).unwrap_err().to_string().contains("only an open proposal"));
    // After it, the autopilot finds less to gain from the same queries.
    let after = db.autopilot(&Options::default()).unwrap();
    assert!(after.held_out.before >= r.held_out.after - 1e-9, "{} vs {}", after.held_out.before, r.held_out.after);
}

#[test]
fn a_proposal_measured_against_another_profile_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (db, docs) = corpus(&dir.path().join("db"));
    log_and_grade(&db, &docs);
    let r = db.autopilot(&Options::default()).unwrap();
    let id = db.keep_proposal(&r).unwrap();
    sql(&db, "CREATE OR REPLACE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY bm25(body, :q) DESC");
    let e = db.apply_proposal(&id, true).unwrap_err().to_string();
    assert!(e.contains("changed since"), "{e}");
    db.set_proposal_status(&id, "dismissed").unwrap();
    assert_eq!(db.proposal(&id, false).unwrap()["status"], "dismissed");
}

#[test]
fn apply_makes_the_proof_again_and_refuses_a_planted_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let (db, docs) = corpus(&dir.path().join("db"));
    log_and_grade(&db, &docs);
    let r = db.autopilot(&Options::default()).unwrap();
    assert_eq!(r.verdict, Verdict::Propose, "{}", r.reason);
    // The proof names the options it was made with.
    assert_eq!(r.to_json()["options"]["min_queries"], 20);
    let id = db.keep_proposal(&r).unwrap();

    // A planted row (audit 4, N-C1): the profile's name and table, open,
    // a verdict to propose and a large gain, whose ranking puts one
    // writer's rows first. Refused, with force too.
    let mut planted = db.proposal(&id, true).unwrap();
    planted["id"] = json!("planted");
    planted["gain"] = json!(0.9);
    planted["statement"] = json!("CREATE OR REPLACE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY CASE WHEN id = 'd7' THEN 1000000000.0 ELSE bm25(body, :q) + 0.05 * bm25(title, :q) END DESC");
    db.add_judgments(autopilot::PROPOSALS, &[planted]).unwrap();
    let e = db.apply_proposal("planted", true).unwrap_err().to_string();
    assert!(e.contains("numbers moved"), "{e}");

    // The grades are gone since it was measured: made again, the proof
    // has too few judged queries, so it is not applied but with force.
    sql(&db, &format!("DELETE FROM {} WHERE grade >= 0", autopilot::JUDGMENTS));
    let e = db.apply_proposal(&id, false).unwrap_err().to_string();
    assert!(e.contains("made again") && e.contains("too_few_judged"), "{e}");
    assert_eq!(db.proposal(&id, false).unwrap()["status"], "open");
    db.apply_proposal(&id, true).unwrap();
    assert_eq!(db.proposal(&id, false).unwrap()["status"], "applied");
}

#[test]
fn it_says_why_when_there_is_nothing_to_go_on() {
    let dir = tempfile::tempdir().unwrap();
    let (db, docs) = corpus(&dir.path().join("db"));
    let r = db.autopilot(&Options::default()).unwrap();
    assert_eq!(r.verdict, Verdict::NoWorkload);
    assert!(r.reason.contains("no rank log"), "{}", r.reason);
    // Queries that rank by no profile.
    let params = Params::new().set("q", Value::Str(word(1)));
    db.query_logged("SELECT id FROM docs ORDER BY bm25(body, :q) DESC LIMIT 10", &params, &Log::default()).unwrap();
    let r = db.autopilot(&Options::default()).unwrap();
    assert_eq!(r.verdict, Verdict::NoWorkload);
    assert!(r.reason.contains("none ranks by a rank profile"), "{}", r.reason);
    // Logged but not graded.
    for w in 0..30 {
        db.query_logged(SQL, &Params::new().set("q", Value::Str(word(w))), &Log::default()).unwrap();
    }
    let r = db.autopilot(&Options::default()).unwrap();
    assert_eq!(r.verdict, Verdict::TooFewJudged, "{}", r.reason);
    // Labels in the rank log are grades too.
    sql(&db, "UPDATE rank_log SET label = 3 WHERE position = 0");
    let r = db.autopilot(&Options { min_queries: 5, ..Options::default() }).unwrap();
    assert!(r.judged >= 30 && r.grades_log >= 30, "{} {}", r.judged, r.grades_log);
    assert_ne!(r.verdict, Verdict::TooFewJudged);
    // A rank log made before the column was added keeps no queries.
    let old = tempfile::tempdir().unwrap();
    let (db2, _) = corpus(&old.path().join("db"));
    sql(&db2, "CREATE TABLE rank_log (id KEYWORD PRIMARY KEY, request KEYWORD NOT NULL, ts TIMESTAMP NOT NULL, context KEYWORD, row_key KEYWORD NOT NULL, position INT NOT NULL, shown BOOL NOT NULL, score FLOAT, features KEYWORD OPTIONS(indexed = false), variant KEYWORD, label FLOAT) OPTIONS(cluster_by = 'context')");
    db2.query_logged(SQL, &Params::new().set("q", Value::Str(word(1))), &Log::default()).unwrap();
    let r = db2.autopilot(&Options::default()).unwrap();
    assert_eq!(r.verdict, Verdict::NoWorkload);
    assert!(r.reason.contains("ADD COLUMN query"), "{}", r.reason);
    drop(docs);
}

#[test]
fn a_default_at_zero_is_tried_too() {
    let dir = tempfile::tempdir().unwrap();
    let (db, docs) = corpus(&dir.path().join("db"));
    sql(&db, "CREATE OR REPLACE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY bm25(body, :q) + :w * bm25(title, :q) DESC WITH (w => 0)");
    log_and_grade(&db, &docs);
    let r = db.autopilot(&Options::default()).unwrap();
    assert_eq!(r.verdict, Verdict::Propose, "{}", r.reason);
    assert_eq!(r.changes.len(), 1, "{:?}", r.changes);
    assert_eq!(r.changes[0].0.knob.to_string(), ":w");
    assert!(r.changes[0].1 > 0.0);
    assert!(r.statement.as_deref().unwrap().contains("WITH (w => "), "{:?}", r.statement);
}
