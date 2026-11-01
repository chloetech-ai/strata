//! ColBERT (feature `bert`): Strata's vectors of a checkpoint's tokens are
//! the ones colbert-ai computes (tests/models/colbert_small and
//! colbert_reference.json, made by make_colbert.py there): documents with
//! their punctuation left out and cut to `doc_maxlen`, queries padded with
//! masks to `query_maxlen`. A multi-vector field embedded with it ranks the
//! rows BM25 finds by `maxsim` against the query's vectors.
#![cfg(feature = "bert")]

use std::path::{Path, PathBuf};

use strata::db::{Db, SqlResult};
use strata::embed::{ColbertEmbedder, Embedder};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, Layout};
use strata::value::Value;

fn models() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models")
}

fn reference() -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(models().join("colbert_reference.json")).unwrap()).unwrap()
}

fn same(what: &serde_json::Value, got: &[Vec<f32>]) {
    let text = what["text"].as_str().unwrap();
    let want: Vec<Vec<f32>> = serde_json::from_value(what["vectors"].clone()).unwrap();
    assert_eq!(got.len(), want.len(), "{text:?}: vectors");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g.len(), w.len());
        let off = g.iter().zip(w).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(off < 1e-4, "{text:?}: token {i} is {off} off");
    }
}

#[test]
fn colbert_vectors_are_colbert_ais() {
    let m = ColbertEmbedder::open(&models().join("colbert_small")).unwrap();
    assert_eq!((m.dim(), m.tokens(), m.single()), (16, true, false));
    let r = reference();
    let docs = r["docs"].as_array().unwrap();
    let texts: Vec<&str> = docs.iter().map(|d| d["text"].as_str().unwrap()).collect();
    let got = m.embed_tokens(&texts).unwrap();
    for (d, g) in docs.iter().zip(&got) {
        same(d, g);
    }
    // One at a time, and in another order, the same.
    for (i, d) in docs.iter().enumerate().rev() {
        same(d, &m.embed_tokens(&[texts[i]]).unwrap()[0]);
    }
    for q in r["queries"].as_array().unwrap() {
        let got = m.embed_query_tokens(q["text"].as_str().unwrap()).unwrap();
        assert_eq!(got.len(), 8, "query_maxlen");
        same(q, &got);
    }
    // The long document is cut to doc_maxlen (24): 24 tokens, less its
    // punctuation.
    let long = got.iter().map(Vec::len).max().unwrap();
    assert!(long <= 24, "{long}");
    let e = m.embed(&["x"]).unwrap_err().to_string();
    assert!(e.contains("a vector a token"), "{e}");
}

#[test]
fn a_colbert_field_ranks_rows_by_maxsim() {
    let dir = tempfile::tempdir().unwrap();
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("d"), seed, DbOptions { layout: Layout::Directory, sync: false, ..DbOptions::default() }).unwrap();
    assert!(db.open_embedders(&models()).is_empty());
    let none = Params::new();
    db.sql("CREATE TABLE d (id KEYWORD PRIMARY KEY, body TEXT, tok VECTOR(16)[] OPTIONS(embed = 'body', model = 'colbert_small', index = 'flat', keep_f32 = true))", &none).unwrap();
    let r = reference();
    let docs = r["docs"].as_array().unwrap();
    for (i, d) in docs.iter().enumerate() {
        db.sql("INSERT INTO d (id, body) VALUES (:id, :body)", &Params::new().set("id", Value::Str(format!("d{i}").into())).set("body", Value::Str(d["text"].as_str().unwrap().into()))).unwrap();
    }
    let rows = |sql: &str, p: &Params| match db.sql(sql, p).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    };
    // Stored as colbert-ai makes them (an empty text has none).
    for (i, d) in docs.iter().enumerate() {
        let v = rows(&format!("SELECT tok FROM d WHERE id = 'd{i}'"), &none).remove(0).remove(0);
        match v {
            Value::Null => assert_eq!(d["text"], "", "d{i}"),
            Value::List(xs) => same(d, &xs.iter().map(|x| if let Value::Vector(v) = x { v.to_vec() } else { panic!("{x:?}") }).collect::<Vec<_>>()),
            v => panic!("{v:?}"),
        }
    }
    // maxsim over the query's vectors, as colbert-ai scores: the sum over
    // its tokens of each one's best cosine in the row.
    let query = r["queries"][0]["text"].as_str().unwrap();
    let q = db.embed_query_in("d", "tok", query).unwrap();
    let p = Params::new().set("q", q.clone()).set("w", Value::Str(query.into()));
    let got = rows("WITH top AS (SELECT * FROM d ORDER BY bm25(body, :w) DESC LIMIT 10) SELECT id, score() FROM top ORDER BY maxsim(tok, :q) DESC LIMIT 10", &p);
    assert!(!got.is_empty());
    let qs: Vec<Vec<f32>> = serde_json::from_value(r["queries"][0]["vectors"].clone()).unwrap();
    for row in &got {
        let i: usize = row[0].as_str().unwrap()[1..].parse().unwrap();
        let ds: Vec<Vec<f32>> = serde_json::from_value(docs[i]["vectors"].clone()).unwrap();
        let want: f32 = qs.iter().map(|q| ds.iter().map(|d| q.iter().zip(d).map(|(a, b)| a * b).sum::<f32>()).fold(f32::MIN, f32::max)).sum();
        assert!((row[1].as_f64().unwrap() - want as f64).abs() < 1e-3, "{row:?}: {want}");
    }
    // A single-vector field cannot take it.
    let e = db.sql("CREATE TABLE s (id KEYWORD PRIMARY KEY, body TEXT, v VECTOR(16) OPTIONS(embed = 'body', model = 'colbert_small'))", &none);
    let e = match e {
        Err(e) => e.to_string(),
        Ok(_) => db.set_embedder_in("s", "v", strata::embed::open_for("colbert_small", Some(16), &models()).unwrap()).unwrap_err().to_string(),
    };
    assert!(e.contains("VECTOR(16)[]"), "{e}");
}
