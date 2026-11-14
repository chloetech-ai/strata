//! Text a language model makes of text (`chloedb::hosted::Generator`): a
//! TEXT field derived from another TEXT field by a model told what to make
//! by the field's prompt, made before the writer through loads, SQL
//! INSERT and UPDATE, kept when a write gives it, made once per distinct
//! text, and read by BM25 as any text is; and what the schema refuses.
//! Against a mock of Anthropic's and a chat server's APIs on loopback.

#![cfg(feature = "hosted")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as J};
use chloedb::db::{Db, SqlResult};
use chloedb::query::Params;
use chloedb::schema::{Field, FieldType, Schema};
use chloedb::storage::DbOptions;
use chloedb::value::Value;

/// A server on loopback that answers each request as a language model
/// would: the first three words of the user's text, upper-cased, after
/// "ABOUT:". The requests' bodies and headers are kept.
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<(String, Vec<(String, String)>, J)>>>,
}

impl Mock {
    fn start(chat: bool) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<(String, Vec<(String, String)>, J)>>> = Arc::default();
        let s = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let s = s.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    let mut headers = Vec::new();
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).unwrap();
                        let h = h.trim_end().to_string();
                        if h.is_empty() {
                            break;
                        }
                        let (k, v) = h.split_once(':').unwrap();
                        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                    }
                    let len: usize = headers.iter().find(|(k, _)| k == "content-length").map_or(0, |(_, v)| v.parse().unwrap());
                    let mut body = vec![0; len];
                    r.read_exact(&mut body).unwrap();
                    let body: J = serde_json::from_slice(&body).unwrap();
                    let user = body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap().to_string();
                    let made = format!("ABOUT: {}", user.split_whitespace().take(3).collect::<Vec<_>>().join(" ").to_uppercase());
                    s.lock().unwrap().push((path, headers, body));
                    let answer = if chat {
                        json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": format!("  {made}\n")}}]})
                    } else {
                        json!({"type": "message", "content": [{"type": "text", "text": made}]})
                    };
                    let text = answer.to_string();
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

const PROMPT: &str = "Name what the note is about, in a few words.";

fn notes(model: &str) -> Schema {
    let text = || FieldType::Text { positions: true };
    Schema::new("notes", vec![Field::new("id", FieldType::Keyword), Field::new("body", text()), Field::new("about", text()).derived_by(model, "body", PROMPT)], Some("id")).unwrap()
}

fn sql(db: &Db, q: &str) -> Vec<Vec<Value>> {
    match db.sql(q, &Params::new()).unwrap_or_else(|e| panic!("{q}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn about(db: &Db, id: &str) -> Option<String> {
    sql(db, &format!("SELECT about FROM notes WHERE id = '{id}' LIMIT 1")).first().and_then(|r| r[0].as_str().map(String::from))
}

fn doc(id: &str, body: &str) -> Vec<Value> {
    vec![Value::Str(id.into()), Value::Str(body.into()), Value::Null]
}

#[test]
fn a_language_model_makes_text_of_text() {
    let mock = Mock::start(false);
    std::env::set_var("ANTHROPIC_BASE_URL", &mock.url);
    std::env::set_var("ANTHROPIC_API_KEY", "ak-derive");
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("n.strata"), notes("anthropic/claude-haiku-5-5"), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    assert!(db.open_embedders(&dir.path().join("models")).is_empty());

    // A load: one request a distinct text.
    db.load(vec![doc("a", "kettle descaling with citric acid"), doc("b", "invoice for the March rent"), doc("c", "kettle descaling with citric acid")]).unwrap();
    assert_eq!(about(&db, "a").as_deref(), Some("ABOUT: KETTLE DESCALING WITH"));
    assert_eq!(about(&db, "c"), about(&db, "a"));
    assert_eq!(about(&db, "b").as_deref(), Some("ABOUT: INVOICE FOR THE"));
    assert_eq!(mock.requests(), 2);
    {
        let seen = mock.seen.lock().unwrap();
        let (path, headers, body) = &seen[0];
        assert_eq!(path, "/messages");
        let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!((h("x-api-key"), h("anthropic-version")), (Some("ak-derive"), Some("2023-06-01")));
        assert_eq!((body["model"].as_str(), body["system"].as_str()), (Some("claude-haiku-5-5"), Some(PROMPT)));
        assert!(body.get("temperature").is_none(), "current Claude models refuse a temperature: {body}");
        assert_eq!(body["max_tokens"], 1024);
    }
    // BM25 reads it.
    let hits = sql(&db, "SELECT id FROM notes WHERE MATCH(about, 'invoice')");
    assert_eq!(hits, vec![vec![Value::Str("b".into())]]);

    // SQL: an INSERT that leaves it out gets it, one that gives it keeps it
    // (no request), an UPDATE of the source makes it again, and one of
    // another field leaves it.
    sql(&db, "INSERT INTO notes (id, body) VALUES ('d', 'garden hose leaks at the tap')");
    assert_eq!(about(&db, "d").as_deref(), Some("ABOUT: GARDEN HOSE LEAKS"));
    let n = mock.requests();
    sql(&db, "INSERT INTO notes (id, body, about) VALUES ('e', 'tax return', 'taxes')");
    assert_eq!((about(&db, "e").as_deref(), mock.requests()), (Some("taxes"), n));
    sql(&db, "UPDATE notes SET body = 'dentist appointment on Friday' WHERE id = 'd'");
    assert_eq!(about(&db, "d").as_deref(), Some("ABOUT: DENTIST APPOINTMENT ON"));
    assert_eq!(mock.requests(), n + 1);
    // An empty text makes an empty one, with no request.
    sql(&db, "INSERT INTO notes (id, body) VALUES ('f', '')");
    assert_eq!(mock.requests(), n + 1);

    // The table says what makes the text, and its CREATE TABLE gives it again.
    let schema = db.schema();
    assert!(schema.features().any(|f| f == "derive_text"), "{:?}", schema.features().collect::<Vec<_>>());
    let create = chloedb::query::ddl::create_sql(&schema);
    assert!(create.contains("about TEXT OPTIONS(derive = 'body', model = 'anthropic/claude-haiku-5-5', prompt = 'Name what the note is about, in a few words.')"), "{create}");
    assert_eq!(schema.to_short_json()["fields"][2]["derive"]["prompt"], PROMPT);
    let chloedb::query::Command::CreateTable { schema: again, .. } = chloedb::query::bind_command(&create, &[], &Params::new()).unwrap() else { panic!("a CREATE TABLE") };
    assert_eq!(again.fields[2].derive, schema.fields[2].derive);
}

#[test]
fn a_chat_server_of_ones_own() {
    let mock = Mock::start(true);
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir_all(models.join("local-llm")).unwrap();
    std::fs::write(models.join("local-llm/endpoint.json"), json!({"api": "chat", "base_url": format!("{}/v1", mock.url), "model": "qwen3-8b", "concurrency": 2}).to_string()).unwrap();
    let db = Db::create(&dir.path().join("n.strata"), notes("local-llm"), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    assert!(db.open_embedders(&models).is_empty());
    db.load((0..5).map(|i| doc(&format!("n{i}"), &format!("note number {i} about bread")))).unwrap();
    assert_eq!(about(&db, "n3").as_deref(), Some("ABOUT: NOTE NUMBER 3"), "the answer is trimmed");
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 5);
    let (path, _, body) = &seen[0];
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(body["messages"][0], json!({"role": "system", "content": PROMPT}));
    assert_eq!(body["model"], "qwen3-8b");
}

#[test]
fn what_text_of_text_takes() {
    let text = || FieldType::Text { positions: true };
    let make = |fields: Vec<Field>| Schema::new("t", fields, Some("id")).map(|_| ()).unwrap_err().to_string();
    let id = || Field::new("id", FieldType::Keyword);
    // A text source without a prompt; a media source with one.
    let e = make(vec![id(), Field::new("a", text()), Field::new("b", text()).derived("anthropic/claude-haiku-5-5", "a")]);
    assert!(e.contains("not a MEDIA field") && e.contains("prompt"), "{e}");
    let e = make(vec![id(), Field::media("img", chloedb::media::MediaKind::Image), Field::new("b", text()).derived_by("anthropic/claude-haiku-5-5", "img", "Describe it")]);
    assert!(e.contains("takes no prompt"), "{e}");
    // A source generated, derived, or of many values.
    let e = make(vec![id(), Field::new("a", text()), Field::new("g", text()).not_stored().generated(&["a"]), Field::new("b", text()).derived_by("anthropic/x", "g", "p")]);
    assert!(e.contains("not generated or derived"), "{e}");
    let e = make(vec![id(), Field::new("k", FieldType::Keyword).multi(), Field::new("b", text()).derived_by("anthropic/x", "k", "p")]);
    assert!(e.contains("of one value"), "{e}");
    // A model that is not a language model.
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("COHERE_API_KEY", "ck");
    let db = Db::create(&dir.path().join("n.strata"), notes("cohere/rerank-v3.5"), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    let failed = db.open_embedders(&dir.path().join("models"));
    assert_eq!(failed.len(), 1);
    assert!(failed[0].1.to_string().contains("a prompt is for a language model"), "{}", failed[0].1);
    // SQL takes the prompt, and only for derived text.
    let e = chloedb::query::bind_command("CREATE TABLE t (id KEYWORD PRIMARY KEY, a TEXT, b TEXT OPTIONS(prompt = 'x'))", &[], &Params::new()).map(|_| ()).unwrap_err().to_string();
    assert!(e.contains("derive = 'text column'"), "{e}");
}
