//! The Model Context Protocol at `POST /v1/mcp` and through `chloedb-server
//! mcp`: its tools are the server's routes, with the caller's key.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use chloedb::db::Db;
use chloedb::storage::DbOptions;
use chloedb_server::auth::{Access, Secret};
use chloedb_server::keys::Keys;
use chloedb_server::service::{AgentConfig, Backend, Service};
use chloedb_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 200;

fn service(dir: &std::path::Path) -> Service {
    let db = Db::create(&dir.join("a.strata"), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(chloedb::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).map(|i| demo::product(i, 7))).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(10));
    svc.keys = Arc::new(Keys::open(dir.join("a.strata.keys.json")).unwrap());
    svc
}

/// A server with a root token, and keys of each scope: its URL, then the
/// read, write and admin keys.
async fn start(dir: &std::path::Path) -> (String, String, String, String) {
    let svc = Arc::new(service(dir));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(svc, l, Access { token: Some(Secret::new("root-token")), ..Default::default() }));
    let mut keys = Vec::new();
    for scope in ["read", "write", "admin"] {
        let (status, j) = post(&base, "/v1/admin/keys", Some("root-token"), json!({"name": format!("agent-{scope}"), "scope": scope})).await;
        assert_eq!(status, 200, "{j}");
        keys.push(j["secret"].as_str().unwrap().to_string());
    }
    let admin = keys.pop().unwrap();
    let write = keys.pop().unwrap();
    let read = keys.pop().unwrap();
    (base, read, write, admin)
}

async fn post(base: &str, path: &str, auth: Option<&str>, body: Json) -> (u16, Json) {
    let c = chloedb_server::http_client().build().unwrap();
    let mut r = c.post(format!("{base}{path}")).json(&body);
    if let Some(a) = auth {
        r = r.bearer_auth(a);
    }
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Json::Null))
}

/// A request's result; panics on a JSON-RPC error.
async fn rpc(base: &str, key: &str, method: &str, params: Json) -> Json {
    let (status, j) = post(base, "/v1/mcp", Some(key), json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params})).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["id"], 7, "{j}");
    assert!(j.get("error").is_none(), "{method}: {j}");
    j["result"].clone()
}

/// A tool's answer: whether it failed, and its text (parsed as JSON when
/// it is).
async fn tool(base: &str, key: &str, name: &str, args: Json) -> (bool, Json) {
    let r = rpc(base, key, "tools/call", json!({"name": name, "arguments": args})).await;
    let text = r["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{r}")).to_string();
    (r["isError"].as_bool().unwrap(), serde_json::from_str(&text).unwrap_or(Json::String(text)))
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_list_query_check_and_change_tables_within_their_keys_scope() {
    let dir = tempfile::tempdir().unwrap();
    let (base, read, write, admin) = start(dir.path()).await;

    // initialize: the version asked for when spoken, the tools and resources.
    let init = rpc(&base, &read, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}})).await;
    assert_eq!(init["protocolVersion"], "2025-03-26");
    assert_eq!(init["serverInfo"]["name"], "chloedb");
    assert!(init["capabilities"]["tools"].is_object() && init["capabilities"]["resources"].is_object(), "{init}");
    assert!(init["instructions"].as_str().unwrap().contains("list_tables"));
    let newest = rpc(&base, &read, "initialize", json!({"protocolVersion": "1999-01-01"})).await;
    assert_eq!(newest["protocolVersion"], chloedb_server::mcp::VERSIONS[0]);
    // A notification is accepted with no answer.
    let (status, j) = post(&base, "/v1/mcp", Some(&read), json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
    assert_eq!((status, j), (202, Json::Null));
    // No key, no tools: the route is the API's.
    assert_eq!(post(&base, "/v1/mcp", None, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await.0, 401);

    let tools = rpc(&base, &read, "tools/list", json!({})).await;
    let names: Vec<&str> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for n in ["list_tables", "describe_table", "query", "explain", "check_sql", "execute", "write", "import", "get_rows", "json_paths", "list_models", "search_docs", "whoami"] {
        assert!(names.contains(&n), "{n} in {names:?}");
    }
    let execute = tools["tools"].as_array().unwrap().iter().find(|t| t["name"] == "execute").unwrap();
    assert_eq!(execute["annotations"]["readOnlyHint"], false);
    assert_eq!(execute["annotations"]["destructiveHint"], true);

    // whoami and list_tables.
    let (failed, me) = tool(&base, &read, "whoami", json!({})).await;
    assert!(!failed && me["scope"] == "read", "{me}");
    let (failed, tables) = tool(&base, &read, "list_tables", json!({})).await;
    assert!(!failed, "{tables}");
    let products = tables["tables"].as_array().unwrap().iter().find(|t| t["table"] == "products").unwrap_or_else(|| panic!("{tables}"));
    assert!(products["sql"].as_str().unwrap().starts_with("CREATE TABLE products"), "{products}");
    assert!(products.get("schema").is_none(), "the full schema is describe_table's: {products}");
    let (failed, described) = tool(&base, &read, "describe_table", json!({"table": "products"})).await;
    assert!(!failed, "{described}");
    assert!(described.to_string().contains("\"price\""), "{described}");

    // query: rows, cut at max_rows, which says so; a write is refused.
    let (failed, q) = tool(&base, &read, "query", json!({"sql": "SELECT id, title FROM products WHERE price > :p ORDER BY price DESC LIMIT 20", "params": {"p": 0}, "max_rows": 5})).await;
    assert!(!failed, "{q}");
    assert_eq!(q["rows"].as_array().unwrap().len(), 5, "{q}");
    assert_eq!(q["truncated"]["shown"], 5, "{q}");
    assert!(q["truncated"]["rows"].as_u64().unwrap() > 5, "{q}");
    let (failed, why) = tool(&base, &read, "query", json!({"sql": "SELECT nope FROM products"})).await;
    assert!(failed, "{why}");
    assert!(why.as_str().unwrap().starts_with("400: "), "{why}");
    // A tool's missing argument is the protocol's error.
    let (_, j) = post(&base, "/v1/mcp", Some(&read), json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "query", "arguments": {}}})).await;
    assert_eq!(j["error"]["code"], -32602, "{j}");
    let (_, j) = post(&base, "/v1/mcp", Some(&read), json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "drop_everything", "arguments": {}}})).await;
    assert_eq!(j["error"]["code"], -32602, "{j}");

    // check_sql binds without running.
    let (failed, c) = tool(&base, &read, "check_sql", json!({"sql": "CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT)"})).await;
    assert!(!failed, "{c}");
    assert_eq!(c["statement"], "create table");
    assert_eq!(c["exists"], false);
    assert!(c["sql"].as_str().unwrap().contains("body TEXT"), "{c}");
    let (failed, c) = tool(&base, &read, "check_sql", json!({"sql": "INSERT INTO products (id, title) VALUES ('x', 'y')"})).await;
    assert!(!failed && c["statement"] == "insert" && c["table"] == "products" && c["rows"] == 1, "{c}");
    let (failed, c) = tool(&base, &read, "check_sql", json!({"sql": "INSERT INTO nowhere (id) VALUES ('x')"})).await;
    assert!(failed, "{c}");
    let (_, after) = tool(&base, &read, "list_tables", json!({})).await;
    assert!(!after.to_string().contains("\"notes\""), "check_sql made no table: {after}");

    // execute: the key's scope decides, as at /v1/sql.
    let insert = json!({"sql": "INSERT INTO products (id, title, category, price, sold) VALUES ('mcp-1', 'A desk lamp from an agent', 'home', 20.0, 3)"});
    let (failed, why) = tool(&base, &read, "execute", insert.clone()).await;
    assert!(failed, "{why}");
    assert!(why.as_str().unwrap().starts_with("403: ") && why.as_str().unwrap().contains("write scope"), "{why}");
    let (failed, done) = tool(&base, &write, "execute", insert).await;
    assert!(!failed, "{done}");
    let create = json!({"sql": "CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT)"});
    assert!(tool(&base, &write, "execute", create.clone()).await.0, "a write key makes no table");
    let (failed, made) = tool(&base, &admin, "execute", create).await;
    assert!(!failed, "{made}");

    // write and get_rows.
    let (failed, w) = tool(&base, &write, "write", json!({"ops": [{"op": "upsert", "table": "notes", "doc": {"id": "n1", "body": "the agent wrote this"}}]})).await;
    assert!(!failed, "{w}");
    let (failed, rows) = tool(&base, &read, "get_rows", json!({"table": "notes", "keys": ["n1", "n2"]})).await;
    assert!(!failed, "{rows}");
    assert!(rows.to_string().contains("the agent wrote this"), "{rows}");
    assert!(tool(&base, &read, "write", json!({"ops": [{"op": "delete", "table": "notes", "key": "n1"}]})).await.0, "a read key writes nothing");
    let (failed, found) = tool(&base, &read, "query", json!({"sql": "SELECT id FROM products WHERE MATCH(body, 'desk lamp agent')"})).await;
    assert!(!failed && found.to_string().contains("mcp-1"), "{found}");

    // import: a CSV's text, or objects; a read key loads nothing.
    let csv = "id,body\nn2,first line of a csv\nn3,\"second, with a comma\"\n";
    assert!(tool(&base, &read, "import", json!({"table": "notes", "data": csv})).await.0, "a read key imports nothing");
    let (failed, imported) = tool(&base, &write, "import", json!({"table": "notes", "data": csv})).await;
    assert!(!failed && imported["written"] == 2, "{imported}");
    let (failed, imported) = tool(&base, &write, "import", json!({"table": "notes", "rows": [{"id": "n4", "body": "an object", "colour": "red"}]})).await;
    assert!(!failed && imported["written"] == 1 && imported.to_string().contains("colour"), "{imported}");
    let (failed, rows) = tool(&base, &read, "get_rows", json!({"table": "notes", "keys": ["n3", "n4"]})).await;
    assert!(!failed && rows.to_string().contains("second, with a comma") && rows.to_string().contains("an object"), "{rows}");
    let (_, j) = post(&base, "/v1/mcp", Some(&write), json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "import", "arguments": {"table": "notes"}}})).await;
    assert_eq!(j["error"]["code"], -32602, "rows are needed: {j}");
    // create: an admin key makes the table from the rows' header and values.
    let gear = "id,title,price,stock\ng-1,Trail Running Shoes,89.5,12\ng-2,Rain Jacket,120,3\n";
    assert!(tool(&base, &write, "import", json!({"table": "gear", "data": gear, "create": true})).await.0, "a write key makes no table");
    let (failed, imported) = tool(&base, &admin, "import", json!({"table": "gear", "data": gear, "create": true})).await;
    assert!(!failed && imported["written"] == 2, "{imported}");
    assert_eq!(imported["create_table"], "CREATE TABLE IF NOT EXISTS gear (id KEYWORD PRIMARY KEY, title TEXT, price FLOAT, stock INT)");
    let (failed, found) = tool(&base, &read, "query", json!({"sql": "SELECT id FROM gear WHERE stock > 5 AND MATCH(title, 'running')"})).await;
    assert!(!failed && found.to_string().contains("g-1") && !found.to_string().contains("g-2"), "{found}");

    // check_sql knows the statements execute takes before SQL's own.
    let (failed, c) = tool(&base, &read, "check_sql", json!({"sql": "CREATE ROLLUP sold_daily ON products AS SELECT date_trunc('day', ts), category, COUNT(*)"})).await;
    assert!(failed && c.as_str().unwrap().contains("no field ts"), "bound against the table: {c}");
    let (failed, c) = tool(&base, &read, "check_sql", json!({"sql": "REFRESH ROLLUP sold_daily FULL"})).await;
    assert!(!failed && c["statement"] == "refresh rollup" && c["full"] == true, "{c}");
    let (failed, c) = tool(&base, &read, "check_sql", json!({"sql": "DROP ROLLUP IF EXISTS sold_daily"})).await;
    assert!(!failed && c["statement"] == "drop rollup", "{c}");

    // A MATCH of words no row has together says what finds them.
    let (failed, none) = tool(&base, &read, "query", json!({"sql": "SELECT id FROM products WHERE MATCH(body, 'desk lamp zeppelin')"})).await;
    assert!(!failed && none["rows"].as_array().unwrap().is_empty(), "{none}");
    let hint = none["hint"].as_str().unwrap_or_else(|| panic!("{none}"));
    assert!(hint.contains("'desk OR lamp OR zeppelin'") && hint.contains("bm25(body, 'desk lamp zeppelin')"), "{hint}");
    let (_, unranked) = tool(&base, &read, "query", json!({"sql": "SELECT id, score() FROM products WHERE MATCH(body, 'lamp') LIMIT 3"})).await;
    assert!(unranked["hint"].as_str().is_some_and(|h| h.contains("ORDER BY bm25")), "{unranked}");
    let (_, ranked) = tool(&base, &read, "query", json!({"sql": "SELECT id FROM products WHERE MATCH(body, 'lamp') ORDER BY bm25(body, 'lamp') DESC LIMIT 3"})).await;
    assert!(ranked.get("hint").is_none(), "{ranked}");
    // `body MATCH 'x'` is refused with the form ChloeDB writes.
    let (failed, why) = tool(&base, &read, "query", json!({"sql": "SELECT id FROM products WHERE body MATCH 'lamp'"})).await;
    assert!(failed && why.as_str().unwrap().contains("MATCH(body, 'lamp')"), "{why}");

    // explain, models, docs.
    let (failed, plan) = tool(&base, &read, "explain", json!({"sql": "SELECT id FROM products WHERE MATCH(body, 'laptop') ORDER BY bm25(body, 'laptop') DESC LIMIT 3"})).await;
    assert!(!failed, "{plan}");
    assert!(!tool(&base, &read, "list_models", json!({})).await.0);
    let (failed, docs) = tool(&base, &read, "search_docs", json!({"query": "rank profile", "limit": 2})).await;
    assert!(!failed, "{docs}");
    let sections = docs["sections"].as_array().unwrap();
    assert!(!sections.is_empty() && sections.len() <= 2, "{docs}");
    assert!(sections[0]["text"].as_str().unwrap().to_lowercase().contains("profile"), "{docs}");

    // resources: the two documents, whole.
    let list = rpc(&base, &read, "resources/list", json!({})).await;
    assert_eq!(list["resources"].as_array().unwrap().len(), 2, "{list}");
    let doc = rpc(&base, &read, "resources/read", json!({"uri": "strata://docs/query-language"})).await;
    assert_eq!(doc["contents"][0]["text"].as_str().unwrap(), chloedb_server::mcp::QUERY_LANGUAGE);

    // A batch is answered as one; the notification in it is not.
    let (status, batch) = post(&base, "/v1/mcp", Some(&read), json!([
        {"jsonrpc": "2.0", "id": "a", "method": "ping"},
        {"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}},
        {"jsonrpc": "2.0", "id": "b", "method": "no/such"}
    ])).await;
    assert_eq!(status, 200);
    assert_eq!(batch.as_array().unwrap().len(), 2, "{batch}");
    assert_eq!(batch[0]["result"], json!({}));
    assert_eq!(batch[1]["error"]["code"], -32601);
    let (status, j) = post(&base, "/v1/mcp", Some(&read), json!("not a message")).await;
    assert_eq!(status, 200);
    assert_eq!(j["error"]["code"], -32600, "{j}");

    // The audit log names the key behind a refused tool's route.
    let c = chloedb_server::http_client().build().unwrap();
    let audit: Json = c.get(format!("{base}/v1/admin/audit")).bearer_auth("root-token").send().await.unwrap().json().await.unwrap();
    let entries = audit["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["who"] == "agent-read" && e["route"] == "/v1/sql" && e["status"] == 403), "{audit}");
    assert!(entries.iter().any(|e| e["who"] == "agent-admin" && e["route"] == "/v1/sql" && e["detail"].as_str().is_some_and(|d| d.starts_with("CREATE TABLE notes"))), "{audit}");
}

/// A browser page on another site may not call the tools (DNS rebinding,
/// or a form's post): the same check as every route.
#[tokio::test(flavor = "multi_thread")]
async fn other_origins_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (base, read, _, _) = start(dir.path()).await;
    let c = chloedb_server::http_client().build().unwrap();
    let r = c.post(format!("{base}/v1/mcp")).bearer_auth(&read).header("origin", "https://evil.example").json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403);
    // There is no stream to open.
    let r = c.get(format!("{base}/v1/mcp")).bearer_auth(&read).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 405);
}

/// The next line the child wrote, within 30 s.
async fn next(out: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>) -> String {
    tokio::time::timeout(Duration::from_secs(30), out.next_line()).await.expect("an answer in time").unwrap().expect("a line")
}

/// `chloedb-server mcp` speaks MCP over stdio: one message a line, sent to
/// the server with the key from CHLOEDB_KEY.
#[tokio::test(flavor = "multi_thread")]
async fn the_stdio_bridge_sends_each_line_to_the_server() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let (base, read, _, _) = start(dir.path()).await;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_chloedb-server"))
        .args(["mcp", "--url", &base])
        .env("CHLOEDB_KEY", &read)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let lines = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "query", "arguments": {"sql": "SELECT COUNT(*) AS n FROM products"}}}),
    ];
    for l in &lines {
        stdin.write_all(format!("{l}\n").as_bytes()).await.unwrap();
    }
    stdin.flush().await.unwrap();
    let first: Json = serde_json::from_str(&next(&mut out).await).unwrap();
    assert_eq!(first["id"], 1);
    assert_eq!(first["result"]["protocolVersion"], "2025-06-18");
    // The notification has no answer: the next line is the query's.
    let second: Json = serde_json::from_str(&next(&mut out).await).unwrap();
    assert_eq!(second["id"], 2, "{second}");
    assert_eq!(second["result"]["isError"], false, "{second}");
    let text: Json = serde_json::from_str(second["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["rows"][0][0], N as u64, "{text}");

    // An unreachable server's answer is an error, not silence.
    drop(stdin);
    let _ = child.wait().await;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_chloedb-server"))
        .args(["mcp", "--url", "http://127.0.0.1:9"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n").await.unwrap();
    stdin.flush().await.unwrap();
    let a: Json = serde_json::from_str(&next(&mut out).await).unwrap();
    assert_eq!(a["id"], 9);
    assert_eq!(a["error"]["code"], -32000, "{a}");
}
