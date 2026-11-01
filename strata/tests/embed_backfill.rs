//! Rows written while an embedded field had no model open are stored
//! without its vector; once the model opens, [`Db::embed_backfill`] fills
//! them in: every row that lacks one and has text, in on-disk segments
//! and the memtable, leaving the rows' other fields, the vectors writers
//! supplied, and rows written in between as they are. A server also fetches the
//! known models a field names that it lacks ([`strata::embed::to_fetch`]).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use strata::db::Db;
use strata::embed::{Embedder, HashEmbedder};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

const DIM: usize = 16;

/// The hash model, counting the texts it embeds.
struct Counting(HashEmbedder, AtomicUsize);

impl Embedder for Counting {
    fn name(&self) -> String {
        self.0.name()
    }
    fn dim(&self) -> usize {
        DIM
    }
    fn embed(&self, texts: &[&str]) -> strata::Result<Vec<Vec<f32>>> {
        self.1.fetch_add(texts.len(), Ordering::Relaxed);
        self.0.embed(texts)
    }
}

/// id, title, stock, emb (the embedding of title).
fn schema() -> Schema {
    Schema::new(
        "docs",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }),
            Field::new("stock", FieldType::Int),
            Field::new("emb", FieldType::Vector { dim: DIM, index: VectorIndexKind::Flat }).not_stored().embedded("hash:16", &["title"]),
        ],
        Some("id"),
    )
    .unwrap()
}

fn doc(id: &str, title: &str, stock: i64) -> Vec<Value> {
    vec![Value::Str(id.into()), Value::Str(title.into()), Value::Int(stock), Value::Null]
}

fn emb_of(db: &Db, key: &str) -> Option<Vec<f32>> {
    let r = db.query("SELECT emb FROM docs WHERE id = :k", &Params::new().set("k", Value::Str(key.into()))).unwrap();
    r.rows.first().and_then(|row| row[0].as_vector().map(|v| v.to_vec()))
}

fn want(text: &str) -> Vec<f32> {
    HashEmbedder { dim: DIM }.embed(&[text]).unwrap().pop().unwrap()
}

fn sorted(mut keys: Vec<Value>) -> Vec<String> {
    let mut out: Vec<String> = keys.drain(..).map(|k| k.as_str().unwrap().to_string()).collect();
    out.sort();
    out
}

#[test]
fn rows_stored_without_vectors_get_them_when_the_model_opens() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("b.strata"), schema(), DbOptions::default()).unwrap();
    // No model: the rows are stored without vectors, except the one
    // whose writer supplied it.
    let mut docs: Vec<_> = (0..300).map(|i| doc(&format!("d{i:03}"), &format!("red mug {i}"), i)).collect();
    docs[1][3] = Value::Vector(vec![1.0; DIM].into());
    docs[2] = doc("d002", "", 2);
    docs[250] = doc("d250", "green teapot", 250);
    db.load(docs[..200].to_vec()).unwrap();
    db.storage().flush().unwrap();
    // Some in an on-disk segment, some in the memtable.
    db.load(docs[200..].to_vec()).unwrap();
    assert!(emb_of(&db, "d000").is_none());
    assert_eq!(db.missing_vectors("docs", "emb").unwrap().len(), 299);

    // Not before the model opens; not for a field that is not embedded.
    assert!(db.embed_backfill("docs", "emb", 64).unwrap_err().to_string().contains("no model open"));
    assert!(db.missing_vectors("docs", "title").is_err());

    let m = Arc::new(Counting(HashEmbedder { dim: DIM }, AtomicUsize::new(0)));
    db.set_embedder("emb", m.clone()).unwrap();
    assert_eq!(db.embed_backfill("docs", "emb", 64).unwrap(), 298);
    assert_eq!(m.1.load(Ordering::Relaxed), 298);
    assert_eq!(emb_of(&db, "d000").unwrap(), want("red mug 0"));
    assert_eq!(emb_of(&db, "d299").unwrap(), want("red mug 299"));
    // The writer's vector stays, the other fields too; the row without
    // text is the one left without a vector.
    assert_eq!(emb_of(&db, "d001").unwrap(), vec![1.0; DIM]);
    let r = db.query("SELECT title, stock FROM docs WHERE id = 'd007'", &Params::new()).unwrap();
    assert_eq!(r.rows[0], vec![Value::Str("red mug 7".into()), Value::Int(7)]);
    assert_eq!(sorted(db.missing_vectors("docs", "emb").unwrap()), ["d002"]);
    assert_eq!(db.query("SELECT COUNT(*) FROM docs", &Params::new()).unwrap().rows[0][0], Value::Int(300));

    // Search by meaning finds them.
    let q = Params::new().set("q", Value::Vector(m.embed_query("green teapot").unwrap().into()));
    let r = db.query("SELECT id FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 1", &q).unwrap();
    assert_eq!(r.rows[0][0], Value::Str("d250".into()));

    // A second run has nothing to do and calls no model.
    let calls = m.1.load(Ordering::Relaxed);
    assert_eq!(db.embed_backfill("docs", "emb", 64).unwrap(), 0);
    assert_eq!(m.1.load(Ordering::Relaxed), calls);
}

#[test]
fn rows_written_after_a_backfill_was_prepared_are_left_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("b.strata"), schema(), DbOptions::default()).unwrap();
    db.load((0..4).map(|i| doc(&format!("d{i}"), &format!("blue kettle {i}"), i))).unwrap();
    db.set_embedder("emb", Arc::new(HashEmbedder { dim: DIM })).unwrap();
    let keys = db.missing_vectors("docs", "emb").unwrap();
    assert_eq!(keys.len(), 4);
    let p = db.prepare_backfill("docs", "emb", keys).unwrap();

    // Between: a writer supplies d0's vector, d1's title changes (and its
    // write embeds it), d2 is deleted.
    let mut d0 = doc("d0", "blue kettle 0", 0);
    d0[3] = Value::Vector(vec![0.5; DIM].into());
    db.storage().begin().and_then(|mut t| { t.upsert(d0)?; t.commit() }).unwrap();
    db.sql("UPDATE docs SET title = 'green teapot' WHERE id = 'd1'", &Params::new()).unwrap();
    db.sql("DELETE FROM docs WHERE id = 'd2'", &Params::new()).unwrap();

    // Only d3 is still without a vector.
    assert_eq!(db.write_prepared(&p).unwrap(), 1);
    assert_eq!(emb_of(&db, "d0").unwrap(), vec![0.5; DIM]);
    assert_eq!(emb_of(&db, "d1").unwrap(), want("green teapot"));
    assert!(emb_of(&db, "d2").is_none());
    assert_eq!(emb_of(&db, "d3").unwrap(), want("blue kettle 3"));
    assert!(db.missing_vectors("docs", "emb").unwrap().is_empty());
}

#[test]
fn a_row_whose_text_changed_meanwhile_gets_the_vector_of_its_new_text() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("b.strata"), schema(), DbOptions::default()).unwrap();
    db.load([doc("d0", "old words", 0)]).unwrap();
    db.set_embedder("emb", Arc::new(HashEmbedder { dim: DIM })).unwrap();
    let p = db.prepare_backfill("docs", "emb", db.missing_vectors("docs", "emb").unwrap()).unwrap();
    // Written again by a writer that does not embed (another Db, a
    // replica's log): still without a vector, with new text.
    db.storage().begin().and_then(|mut t| { t.upsert(doc("d0", "new words", 5))?; t.commit() }).unwrap();
    assert!(emb_of(&db, "d0").is_none());
    assert_eq!(db.write_prepared(&p).unwrap(), 1);
    assert_eq!(emb_of(&db, "d0").unwrap(), want("new words"));
    let r = db.query("SELECT stock FROM docs WHERE id = 'd0'", &Params::new()).unwrap();
    assert_eq!(r.rows[0][0], Value::Int(5));
}

#[test]
fn a_server_fetches_the_known_models_a_field_names_that_are_not_here() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(strata::embed::to_fetch("potion-base-8M", dir.path()), Some("potion-base-8M"));
    assert_eq!(strata::embed::to_fetch("POTION-base-8m", dir.path()), Some("potion-base-8M"));
    // Not models Strata fetches.
    for name in ["hash:16", "openai/text-embedding-3-small", "someone/else", "my-model"] {
        assert_eq!(strata::embed::to_fetch(name, dir.path()), None, "{name}");
    }
    // Nor those already here.
    let m = dir.path().join("potion-base-8M");
    std::fs::create_dir_all(&m).unwrap();
    for f in strata::embed::model_files("potion-base-8M") {
        std::fs::write(m.join(f), b"").unwrap();
    }
    assert_eq!(strata::embed::to_fetch("potion-base-8M", dir.path()), None);
    // Nor those this build cannot run.
    if !cfg!(feature = "vision") {
        assert_eq!(strata::embed::to_fetch("siglip-base-patch16-224", dir.path()), None);
    }
}
