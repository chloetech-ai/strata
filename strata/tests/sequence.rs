//! Sequence encoders (`encode(model, rows)`): the Rust encoder against
//! PyTorch's vectors of the same model (tests/models/make_sequence.py).

use strata::query::sequence::{Event, SequenceModel};

fn reference() -> serde_json::Value {
    serde_json::from_str(include_str!("models/sequence_reference.json")).unwrap()
}

fn small() -> SequenceModel {
    SequenceModel::parse("seq_small", include_bytes!("models/seq_small/config.json"), include_bytes!("models/seq_small/model.safetensors")).unwrap()
}

#[test]
fn equal_to_pytorch() {
    let r = reference();
    let m = small();
    assert_eq!((m.table.as_str(), m.field.as_str(), m.dim, m.max_len), ("items", "emb", 8, 12));
    let items: std::collections::HashMap<String, Vec<f32>> = serde_json::from_value(r["items"].clone()).unwrap();
    let mut checked = 0;
    for c in r["cases"].as_array().unwrap() {
        let h: Vec<(String, Option<String>, Option<f64>)> = serde_json::from_value(c["history"].clone()).unwrap();
        let events: Vec<Event> = h.iter().map(|(i, a, t)| Event { item: &items[i], action: a.as_deref(), time: *t }).collect();
        let got = m.encode(&events, c["now"].as_f64()).unwrap();
        let want: Vec<f32> = serde_json::from_value(c["vector"].clone()).unwrap();
        let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(worst < 2e-5, "{} events, now {}: off by {worst}\n{got:?}\n{want:?}", h.len(), c["now"]);
        checked += 1;
    }
    assert_eq!(checked, 14);
    assert!(m.encode(&[], None).is_none());
}

#[test]
fn a_model_that_does_not_match_its_config_is_refused() {
    let cfg = include_str!("models/seq_small/config.json");
    let w = include_bytes!("models/seq_small/model.safetensors");
    for (from, to, says) in [
        ("\"width\": 16", "\"width\": 8", "shape"),
        ("\"layers\": 2", "\"layers\": 3", "no tensor layers.2"),
        ("\"layers\": 2", "\"layers\": 1", "does not name"),
        ("\"kind\": \"sequence\"", "\"kind\": \"gbdt\"", "kind"),
        ("\"heads\": 2", "\"heads\": 3", "multiple of heads"),
    ] {
        assert!(cfg.contains(from), "{from}");
        let e = SequenceModel::parse("m", cfg.replace(from, to).as_bytes(), w).unwrap_err().to_string();
        assert!(e.contains(says) && e.contains("sequence model m"), "{to}: {e}");
    }
    let e = SequenceModel::parse("m", cfg.as_bytes(), &w[..w.len() - 4]).unwrap_err().to_string();
    assert!(e.contains("bytes for shape"), "{e}");
}

mod query {
    use std::collections::HashMap;
    use std::path::Path;

    use strata::db::Db;
    use strata::query::Params;
    use strata::schema::Schema;
    use strata::storage::DbOptions;
    use strata::value::Value;

    /// Items with the reference's vectors, and events of user `u<case>` for
    /// each reference case with every time known: its history, in order.
    fn data(dir: &Path) -> (Db, serde_json::Value) {
        let r = super::reference();
        let schema = Schema::from_json(r#"{"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "emb", "type": "vector", "dim": 8, "index": "flat"}, {"name": "other", "type": "vector", "dim": 8, "index": "flat"}]}"#).unwrap();
        let db = Db::create(&dir.join("db"), schema, DbOptions { segment_max_docs: 12, memtable_max_docs: 6, ..DbOptions::default() }).unwrap();
        db.open_embedders(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models"));
        db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, seq INT, item KEYWORD, action KEYWORD, ts TIMESTAMP, micros INT, none_k KEYWORD, none_t TIMESTAMP)", &Params::new()).unwrap();
        for (k, v) in r["items"].as_object().unwrap() {
            let v: Vec<f32> = serde_json::from_value(v.clone()).unwrap();
            db.sql("INSERT INTO items (id, emb) VALUES (:id, :v)", &Params::new().set("id", Value::Str(k.clone())).set("v", Value::Vector(v.into()))).unwrap();
        }
        let mut id = 0;
        for (c, case) in r["cases"].as_array().unwrap().iter().enumerate() {
            for (seq, e) in case["history"].as_array().unwrap().iter().enumerate() {
                let p = Params::new()
                    .set("id", Value::Int(id))
                    .set("u", Value::Str(format!("u{c}")))
                    .set("s", Value::Int(seq as i64))
                    .set("i", Value::Str(e[0].as_str().unwrap().into()))
                    .set("a", e[1].as_str().map_or(Value::Null, |a| Value::Str(a.into())))
                    .set("t", e[2].as_f64().map_or(Value::Null, |t| Value::Timestamp((t * 1e6).round() as i64)))
                    .set("micros", e[2].as_f64().map_or(Value::Null, |t| Value::Int((t * 1e6).round() as i64)));
                db.sql("INSERT INTO events (id, usr, seq, item, action, ts, micros) VALUES (:id, :u, :s, :i, :a, :t, :micros)", &p).unwrap();
                id += 1;
            }
        }
        (db, r)
    }

    fn dot(a: &[f32], b: &[f32]) -> f64 {
        a.iter().zip(b).map(|(x, y)| (x * y) as f64).sum()
    }

    #[test]
    fn similarity_to_a_history_ranks_items_as_pytorch_does() {
        let dir = tempfile::tempdir().unwrap();
        let (db, r) = data(dir.path());
        let items: HashMap<String, Vec<f32>> = serde_json::from_value(r["items"].clone()).unwrap();
        let mut checked = 0;
        for (c, case) in r["cases"].as_array().unwrap().iter().enumerate() {
            let want: Vec<f32> = serde_json::from_value(case["vector"].clone()).unwrap();
            let now = case["now"].as_f64();
            let p = Params::new().set("u", Value::Str(format!("u{c}"))).set("now", now.map_or(Value::Null, |t| Value::Timestamp((t * 1e6).round() as i64)));
            let history = "(SELECT item, action, ts FROM events WHERE usr = :u ORDER BY seq DESC LIMIT 200)";
            for sql in [
                format!("SELECT id, score() FROM items ORDER BY similarity(emb, encode('seq_small', {history}, now => :now)) DESC LIMIT 10"),
                format!("SELECT id, score() FROM items ORDER BY maxsim(emb, encode('seq_small', {history}, now => :now)) DESC LIMIT 10"),
            ] {
                if now.is_none() {
                    // Without now, the clock's time: another gap for the newest event.
                    continue;
                }
                let rs = db.query(&sql, &p).unwrap();
                let mut exact: Vec<(f64, &String)> = items.iter().map(|(k, v)| (dot(v, &want), k)).collect();
                exact.sort_by(|a, b| b.0.total_cmp(&a.0));
                let got: Vec<(String, f64)> = rs.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect();
                assert_eq!(got.len(), 10, "{sql}");
                for ((k, s), (e, ek)) in got.iter().zip(&exact) {
                    assert!((s - e).abs() < 1e-4, "case {c}: {k} {s} vs {ek} {e}");
                    assert_eq!(dot(&items[k], &want), dot(&items[*ek], &want), "case {c}");
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 14);
    }

    #[test]
    fn encode_in_expressions_and_its_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = data(dir.path());
        let p = Params::new().set("u", Value::Str("u5".into())).set("now", Value::Timestamp(1_800_000_000_000_000));
        let history = "(SELECT item, action, ts FROM events WHERE usr = :u ORDER BY seq DESC LIMIT 200)";
        // A vector over rows in memory: dot() with each item's.
        let a = db.query(&format!("SELECT id, dot(emb, encode('seq_small', {history}, now => :now)) AS s FROM items ORDER BY s DESC LIMIT 3"), &p).unwrap();
        // Once, though the column and the ORDER BY both name it.
        assert_eq!(a.stats.plan.iter().filter(|l| l.starts_with("encode: seq_small over")).count(), 1, "{:?}", a.stats.plan);
        let b = db.query(&format!("SELECT id, score() FROM items ORDER BY similarity(emb, encode('seq_small', {history}, now => :now)) DESC LIMIT 3"), &p).unwrap();
        for (x, y) in a.rows.iter().zip(&b.rows) {
            assert_eq!(x[0], y[0]);
            assert!((x[1].as_f64().unwrap() - y[1].as_f64().unwrap()).abs() < 1e-5);
        }
        // Two columns: the item and a time (a TIMESTAMP or microseconds), or the
        // item and an action; three with no action, or no time, are the same.
        let top = |cols: &str| {
            let sql = format!("SELECT id, score() FROM items ORDER BY similarity(emb, encode('seq_small', (SELECT {cols} FROM events WHERE usr = :u ORDER BY seq DESC LIMIT 200), now => :now)) DESC LIMIT 5");
            db.query(&sql, &p).unwrap().rows
        };
        assert_eq!(top("item, ts"), top("item, none_k, ts"));
        assert_eq!(top("item, micros"), top("item, none_k, micros"));
        assert_eq!(top("item, micros"), top("item, ts"));
        assert_eq!(top("item, action"), top("item, action, none_t"));
        assert_ne!(top("item, ts"), top("item, action"));
        assert_ne!(top("item, ts"), top("item, action, ts"));
        // A user without events: no vector, no rows.
        let none = Params::new().set("u", Value::Str("nobody".into())).set("now", Value::Int(0));
        assert!(db.query(&format!("SELECT id FROM items ORDER BY similarity(emb, encode('seq_small', {history}, now => :now)) DESC LIMIT 3"), &none).unwrap().rows.is_empty());
        let plan = db.explain(&format!("SELECT id FROM items ORDER BY similarity(emb, encode('seq_small', {history}, now => :now)) DESC LIMIT 3"), &p).unwrap();
        assert!(plan.features.iter().any(|f| f == "encode"), "{:?}", plan.features);
        let text = plan.explain(&db.catalog().iter().map(|t| &**t).collect::<Vec<_>>()).join("\n");
        assert!(text.contains("encode('seq_small', ") && text.contains("now => 1800000000"), "{text}");
        let json = strata::query::ir::Plan::from_json(&plan.to_json().unwrap()).unwrap();
        assert_eq!(json, plan);
        for (sql, says) in [
            (format!("SELECT id FROM items ORDER BY similarity(emb, encode('nope', {history})) DESC LIMIT 3"), "no sequence model named nope"),
            (format!("SELECT id FROM items ORDER BY distance(emb, encode('seq_small', {history})) LIMIT 3"), "not by distance"),
            ("SELECT id FROM items ORDER BY similarity(emb, encode('seq_small', (SELECT item, action, ts, seq FROM events WHERE usr = :u))) DESC LIMIT 3".into(), "one to three columns"),
            ("SELECT id FROM items ORDER BY similarity(emb, encode('seq_small', 'i1')) DESC LIMIT 3".into(), "a subquery of its events"),
            (format!("SELECT id FROM items ORDER BY similarity(emb, encode('seq_small', {history}, now => 'soon')) DESC LIMIT 3"), "now is a time"),
            (format!("SELECT id FROM items ORDER BY similarity(emb, encode('seq_small', {history}, at => 1)) DESC LIMIT 3"), "now => a time"),
            // A field of the dimension, but not the one the model was trained on.
            (format!("SELECT id FROM items ORDER BY similarity(other, encode('seq_small', {history})) DESC LIMIT 3"), "gives vectors of items.emb, not of items.other"),
            (format!("SELECT id FROM items ORDER BY maxsim(other, encode('seq_small', {history})) DESC LIMIT 3"), "gives vectors of items.emb, not of items.other"),
        ] {
            let e = db.query(&sql, &p).unwrap_err().to_string();
            assert!(e.contains(says), "{sql}: {e}");
        }
    }

    #[test]
    fn a_field_of_several_vectors_an_item_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let schema = Schema::from_json(r#"{"table": "seed", "key": "id", "fields": [{"name": "id", "type": "keyword"}]}"#).unwrap();
        let db = Db::create(&dir.path().join("db"), schema, DbOptions::default()).unwrap();
        db.open_embedders(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models"));
        db.sql("CREATE TABLE items (id KEYWORD PRIMARY KEY, emb VECTOR(8)[] OPTIONS(index = 'flat'))", &Params::new()).unwrap();
        db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD)", &Params::new()).unwrap();
        db.sql("INSERT INTO events (id, usr, item) VALUES (1, 'u', 'i1')", &Params::new()).unwrap();
        let sql = "SELECT id FROM items ORDER BY maxsim(emb, encode('seq_small', (SELECT item FROM events WHERE usr = 'u'))) DESC LIMIT 3";
        let e = db.query(sql, &Params::new()).unwrap_err().to_string();
        assert!(e.contains("holds several"), "{e}");
    }
}
