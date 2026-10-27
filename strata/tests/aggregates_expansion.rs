//! Aggregates (`SUM`, `AVG`, `MIN`, `MAX`, `COUNT(DISTINCT)`, several
//! group fields, `HAVING`) against the same computed over the documents,
//! and searches widened by typos and synonyms.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use strata::data::{self, col};
use strata::db::Db;
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::text::{analyze, Synonyms};
use strata::value::Value;
use strata::{Document, VectorIndexKind};

const N: usize = 2000;

fn open(dir: &std::path::Path, docs: &[Document]) -> Db {
    let opts = DbOptions { segment_max_docs: 600, ..DbOptions::default() };
    let db = Db::create(&dir.join("a.strata"), data::beir_schema(8, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")).rows
}

#[test]
fn aggregates_match_the_documents() {
    let docs = data::synthetic(N, 8, 21);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &docs);

    // Per category: rows, average rating, year range, distinct topics.
    #[derive(Default)]
    struct G {
        n: i64,
        rating: f64,
        min_year: i64,
        max_year: i64,
        topics: BTreeSet<String>,
    }
    let mut want: BTreeMap<String, G> = BTreeMap::new();
    for d in docs.iter().filter(|d| d[col::YEAR].as_i64().unwrap() >= 2000) {
        let g = want.entry(d[col::CATEGORY].as_str().unwrap().to_string()).or_insert_with(|| G { min_year: i64::MAX, max_year: i64::MIN, ..G::default() });
        g.n += 1;
        g.rating += d[col::RATING].as_f64().unwrap();
        let y = d[col::YEAR].as_i64().unwrap();
        g.min_year = g.min_year.min(y);
        g.max_year = g.max_year.max(y);
        g.topics.insert(d[col::TOPIC].as_str().unwrap().to_string());
    }
    let got = rows(
        &db,
        "SELECT category, COUNT(*), AVG(rating) AS avg_rating, MIN(year), MAX(year), COUNT(DISTINCT topic) FROM docs \
         WHERE year >= 2000 GROUP BY category ORDER BY category",
    );
    assert_eq!(got.len(), want.len());
    for (r, (cat, g)) in got.iter().zip(&want) {
        assert_eq!(r[0].as_str(), Some(cat.as_str()));
        assert_eq!(r[1], Value::Int(g.n));
        assert!((r[2].as_f64().unwrap() - g.rating / g.n as f64).abs() < 1e-9, "{cat}");
        assert_eq!((r[3].clone(), r[4].clone()), (Value::Int(g.min_year), Value::Int(g.max_year)));
        assert_eq!(r[5], Value::Int(g.topics.len() as i64));
    }

    // HAVING and ORDER BY an aggregate.
    let big: Vec<(&String, i64)> = {
        let mut v: Vec<(&String, i64)> = want.iter().filter(|(_, g)| g.n > 20).map(|(c, g)| (c, g.n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v
    };
    let got = rows(&db, "SELECT category, COUNT(*) AS n FROM docs WHERE year >= 2000 GROUP BY category HAVING n > 20 ORDER BY n DESC, category");
    let got: Vec<(String, i64)> = got.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect();
    assert_eq!(got, big.iter().map(|(c, n)| (c.to_string(), *n)).collect::<Vec<_>>());
    let got = rows(&db, "SELECT category, AVG(rating) FROM docs WHERE year >= 2000 GROUP BY category HAVING COUNT(*) > 20 ORDER BY AVG(rating) DESC LIMIT 3");
    assert_eq!(got.len(), 3.min(big.len()));
    assert!(got.windows(2).all(|w| w[0][1].as_f64() >= w[1][1].as_f64()));

    // Two group fields, an INT group field, and aggregates without groups.
    let pairs: BTreeSet<(String, String)> = docs.iter().map(|d| (d[col::CATEGORY].as_str().unwrap().into(), d[col::TOPIC].as_str().unwrap().into())).collect();
    let got = rows(&db, "SELECT category, topic, COUNT(*) FROM docs GROUP BY category, topic");
    assert_eq!(got.len(), pairs.len());
    assert_eq!(got.iter().map(|r| r[2].as_i64().unwrap()).sum::<i64>(), N as i64);
    assert!(got.windows(2).all(|w| w[0][2].as_i64() >= w[1][2].as_i64()), "largest groups first without ORDER BY");
    let got = rows(&db, "SELECT year, COUNT(*) FROM docs GROUP BY year ORDER BY year DESC LIMIT 3");
    assert_eq!(got[0][0], Value::Int(docs.iter().map(|d| d[col::YEAR].as_i64().unwrap()).max().unwrap()));
    let total: i64 = docs.iter().map(|d| d[col::YEAR].as_i64().unwrap()).sum();
    let got = rows(&db, "SELECT COUNT(*), SUM(year), MIN(category), COUNT(DISTINCT category) FROM docs");
    let cats: BTreeSet<&str> = docs.iter().map(|d| d[col::CATEGORY].as_str().unwrap()).collect();
    assert_eq!(got, vec![vec![Value::Int(N as i64), Value::Int(total), Value::Str(cats.iter().next().unwrap().to_string()), Value::Int(cats.len() as i64)]]);
    // No rows: one group, with NULL averages.
    assert_eq!(rows(&db, "SELECT COUNT(*), AVG(rating) FROM docs WHERE year > 3000"), vec![vec![Value::Int(0), Value::Null]]);

    for bad in [
        "SELECT category, year FROM docs GROUP BY category",
        "SELECT SUM(category) FROM docs",
        "SELECT id FROM docs HAVING COUNT(*) > 1",
        "SELECT body, COUNT(*) FROM docs GROUP BY body",
    ] {
        assert!(db.query(bad, &Params::new()).is_err(), "{bad}");
    }
}

/// A word of the collection and the documents whose body has its term.
fn some_word(docs: &[Document]) -> (String, String) {
    let title = docs[5][col::TITLE].as_str().unwrap();
    let word = title.split(' ').max_by_key(|w| w.len()).unwrap().to_string();
    let term = analyze(&word).pop().unwrap();
    (word, term)
}

fn count(db: &Db, sql: &str, params: &Params) -> i64 {
    db.query(sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")).rows[0][0].as_i64().unwrap()
}

#[test]
fn typos_find_the_word() {
    let docs = data::synthetic(N, 8, 22);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &docs);
    let (word, _) = some_word(&docs);
    // Swap two letters in the middle.
    let mut chars: Vec<char> = word.chars().collect();
    chars.swap(2, 3);
    let typo: String = chars.into_iter().collect();
    assert_ne!(typo, word);
    let p = Params::new().set("w", Value::Str(word.clone())).set("typo", Value::Str(typo.clone()));
    let exact = count(&db, "SELECT COUNT(*) FROM docs WHERE MATCH(body, :w)", &p);
    assert!(exact > 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM docs WHERE MATCH(body, :typo)", &p), 0, "{typo} is not a word");
    let fuzzy = count(&db, "SELECT COUNT(*) FROM docs WHERE MATCH(body, :typo, fuzzy => 1)", &p);
    assert!(fuzzy >= exact, "{typo}: {fuzzy} rows, {word}: {exact}");
    let inline = count(&db, &format!("SELECT COUNT(*) FROM docs WHERE MATCH(body, '{typo}~1')"), &p);
    assert_eq!(inline, fuzzy);
    // Ranked: the typo finds the documents of the word, ranked like it.
    let want = db.query("SELECT id FROM docs ORDER BY bm25(body, :w) DESC LIMIT 5", &p).unwrap();
    let got = db.query("SELECT id FROM docs ORDER BY bm25(body, :typo, fuzzy => 1) DESC LIMIT 5", &p).unwrap();
    assert!(!got.rows.is_empty());
    assert!(got.stats.plan.iter().any(|l| l.contains("expanded")), "{:?}", got.stats.plan);
    let overlap = got.keys.iter().filter(|k| want.keys.contains(k)).count();
    assert!(overlap >= 3, "{:?} vs {:?}", got.keys, want.keys);
}

#[test]
fn synonyms_widen_searches() {
    let docs = data::synthetic(N, 8, 23);
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(dir.path(), &docs);
    let (word, _) = some_word(&docs);
    let other = docs[9][col::TITLE].as_str().unwrap().split(' ').max_by_key(|w| w.len()).unwrap().to_string();
    assert_ne!(analyze(&word), analyze(&other));
    let p = Params::new().set("w", Value::Str(word.clone()));
    assert!(db.query("SELECT COUNT(*) FROM docs WHERE MATCH(body, :w, synonyms => 1)", &p).is_err(), "no synonym list yet");
    db.exec = ExecOptions { synonyms: Some(Arc::new(Synonyms::parse(&format!("{word}, {other}\n")).unwrap())), ..ExecOptions::default() };
    let either = count(&db, &format!("SELECT COUNT(*) FROM docs WHERE MATCH(body, '{word} OR {other}')"), &p);
    let syn = count(&db, "SELECT COUNT(*) FROM docs WHERE MATCH(body, :w, synonyms => 1)", &p);
    assert_eq!(syn, either);
    assert!(syn > count(&db, "SELECT COUNT(*) FROM docs WHERE MATCH(body, :w)", &p));
    let r = db.query("SELECT id FROM docs ORDER BY bm25(body, :w, synonyms => 1) DESC LIMIT 10", &p).unwrap();
    assert!(r.stats.plan.iter().any(|l| l.contains("expanded")), "{:?}", r.stats.plan);
    // A row with only the synonym now scores.
    let only_other = db.query(&format!("SELECT id FROM docs WHERE MATCH(body, '{other} -{word}') LIMIT 1"), &p).unwrap().keys[0].clone();
    let all = db.query("SELECT id FROM docs ORDER BY bm25(body, :w, synonyms => 1) DESC LIMIT 2000", &p).unwrap();
    assert!(all.keys.contains(&only_other));
}
