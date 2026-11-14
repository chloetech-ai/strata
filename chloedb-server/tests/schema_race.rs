//! Writes that race with `ALTER TABLE` on their table (finding PRF-3):
//! each applies against the columns of the moment, since it names only
//! columns the table keeps, through `/v1/write` and as an SQL `INSERT`.
//! A write once read the table's columns before it waited for the writer
//! and was checked against them after, so one that waited out an `ADD
//! COLUMN` or a `DROP COLUMN` was refused as a schema error ("document has
//! 4 values, table has 3 fields"), not to be sent again; an `INSERT` was
//! refused with "table race was altered ... run it again".

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use chloedb::db::Db;
use chloedb::storage::DbOptions;
use chloedb_server::service::{AgentConfig, Backend, Service};
use chloedb_server::{demo, server};
use tokio::net::TcpListener;

async fn post(base: &str, path: &str, body: Json) -> (u16, Json) {
    let r = chloedb_server::http_client().build().unwrap().post(format!("{base}{path}")).json(&body).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_racing_alter_table_apply() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create(&dir.path().join("r.strata"), demo::schema(10), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap());
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let svc = Arc::new(Service::new(Backend::Local(db), schema, agent, 16, Duration::from_secs(10)));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(svc, l, Default::default()));
    let (s, b) = post(&base, "/v1/sql", json!({"sql": "CREATE TABLE race (id INT PRIMARY KEY, a TEXT, n INT)"})).await;
    assert_eq!(s, 200, "{b}");
    let stop = Instant::now() + Duration::from_secs(4);
    let writers: Vec<_> = (0..4)
        .map(|w: i64| {
            let base = base.clone();
            tokio::spawn(async move {
                let (mut refused, mut written) = (Vec::new(), 0);
                let mut i = 0;
                while Instant::now() < stop {
                    let ids: Vec<i64> = (0..20).map(|k| w * 10_000_000 + i + k).collect();
                    i += 20;
                    // Half the writers write operations, half SQL.
                    let answer = match w % 2 {
                        0 => post(&base, "/v1/write", json!({"ops": ids.iter().map(|x| json!({"op": "upsert", "table": "race", "doc": {"id": x, "a": format!("row {x}"), "n": x % 7}})).collect::<Vec<_>>()})).await,
                        _ => post(&base, "/v1/sql", json!({"sql": format!("INSERT INTO race (id, a, n) VALUES {}", ids.iter().map(|x| format!("({x}, 'row {x}', {})", x % 7)).collect::<Vec<_>>().join(", "))})).await,
                    };
                    match answer {
                        (200, _) => written += ids.len() as i64,
                        (s, b) => refused.push(format!("{s} {b}")),
                    }
                }
                (refused, written)
            })
        })
        .collect();
    let ddl = {
        let base = base.clone();
        tokio::spawn(async move {
            let mut k = 0;
            while Instant::now() < stop {
                k += 1;
                for sql in [format!("ALTER TABLE race ADD COLUMN c{k} INT"), format!("ALTER TABLE race RENAME COLUMN c{k} TO d{k}"), format!("ALTER TABLE race DROP COLUMN d{k}")] {
                    let (s, b) = post(&base, "/v1/sql", json!({"sql": sql})).await;
                    assert_eq!(s, 200, "{sql}: {b}");
                }
            }
            k
        })
    };
    let mut refused = Vec::new();
    let mut written = 0;
    for w in writers {
        let (r, n) = w.await.unwrap();
        refused.extend(r);
        written += n;
    }
    let changes = ddl.await.unwrap();
    assert!(written > 0 && changes > 0, "{written} writes, {changes} rounds of changes");
    assert!(refused.is_empty(), "{} writes refused ({written} rows written), first: {}", refused.len(), refused[0]);
    let (s, b) = post(&base, "/v1/query", json!({"sql": "SELECT COUNT(*) FROM race"})).await;
    assert_eq!((s, b["rows"][0][0].as_i64()), (200, Some(written)), "{b}");
}
