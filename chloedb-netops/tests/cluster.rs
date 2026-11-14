//! chloedb-netops against a real chloedb-server: `serve --local` starts one,
//! builds the network in it, and the test reads it the way the page does,
//! breaks it, checks what the rules and the agents made of that, approves an
//! action, and reads everything back after a restart.
//!
//! It needs the chloedb-server binary (`CHLOEDB_SERVER_BIN`, else
//! `../chloedb-server/target/{ci,release}/chloedb-server`), and is skipped
//! without it unless `CHLOEDB_NETOPS_NEED_SERVER` is set (as CI does).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TOKEN: &str = "netops-test-token";

fn server_bin() -> Option<PathBuf> {
    if let Some(b) = std::env::var_os("CHLOEDB_SERVER_BIN") {
        return Some(PathBuf::from(b));
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../chloedb-server/target");
    ["ci", "release"].iter().map(|p| root.join(p).join("chloedb-server")).find(|p| p.exists())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct App {
    child: Child,
    url: String,
    /// The chloedb-server it started, where a real feed would write.
    strata: String,
}

impl App {
    fn start(dir: &std::path::Path, bin: &std::path::Path) -> App {
        App::start_with(dir, bin, &[])
    }

    fn start_with(dir: &std::path::Path, bin: &std::path::Path, more: &[&str]) -> App {
        let (port, strata) = (free_port(), free_port());
        let child = Command::new(env!("CARGO_BIN_EXE_chloedb-netops"))
            .args(["serve", "--local"])
            .arg(dir)
            .arg("--server-bin")
            .arg(bin)
            .args(["--listen", &format!("127.0.0.1:{port}"), "--local-listen", &format!("127.0.0.1:{strata}"), "--token", TOKEN])
            // No faults of its own, and a short history: the test injects what it needs.
            .args(["--fault-every", "0", "--history-hours", "2", "--estate", "4000", "--rewrite-model", ""])
            .args(more)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("chloedb-netops");
        App { child, url: format!("http://127.0.0.1:{port}"), strata: format!("http://127.0.0.1:{strata}") }
    }

    /// SIGTERM: it stops its chloedb-server and exits.
    fn stop(mut self) {
        // SAFETY: a signal to a child this test started.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(30) {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        panic!("chloedb-netops did not stop on SIGTERM");
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // After a failed assertion too, it is asked to stop first, so that
        // it takes the chloedb-server it started with it.
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: a signal to a child this test started.
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(10) && self.child.try_wait().ok().flatten().is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let _ = self.child.kill();
    }
}

struct Api {
    http: reqwest::Client,
    url: String,
}

impl Api {
    /// Waits until the network is built and the first minute was evaluated.
    async fn ready(url: &str) -> Api {
        let http = reqwest::Client::new();
        let t0 = Instant::now();
        loop {
            if let Ok(r) = http.get(format!("{url}/v1/status")).bearer_auth(TOKEN).send().await {
                if let Ok(j) = r.json::<Value>().await {
                    assert!(j["error"].is_null() || j["ready"] == true, "setup failed: {}", j["error"]);
                    if j["ready"] == true {
                        break;
                    }
                }
            }
            assert!(t0.elapsed() < Duration::from_secs(180), "chloedb-netops did not become ready");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Api { http, url: url.to_string() }
    }
    async fn get(&self, path: &str) -> Value {
        let r = self.http.get(format!("{}{path}", self.url)).bearer_auth(TOKEN).send().await.unwrap();
        let status = r.status();
        let body = r.text().await.unwrap();
        assert!(status.is_success(), "GET {path}: {status} {body}");
        serde_json::from_str(&body).unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        let r = self.http.post(format!("{}{path}", self.url)).bearer_auth(TOKEN).json(&body).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "POST {path}: {status} {text}");
        serde_json::from_str(&text).unwrap()
    }
}

fn len(v: &Value) -> usize {
    v.as_array().map(Vec::len).unwrap_or(0)
}

/// Every answer says which SQL it ran, and none of it was refused.
fn ran_sql(j: &Value, what: &str) {
    let qs = j["queries"].as_array().unwrap_or_else(|| panic!("{what}: no queries listed"));
    assert!(!qs.is_empty(), "{what}: no queries listed");
    for q in qs {
        assert!(q["sql"].as_str().is_some_and(|s| s.contains("netops_")), "{what}: a query without SQL: {q}");
        assert!(q["note"].as_str().unwrap_or("").is_empty(), "{what}: a query was refused: {q}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_network_built_broken_diagnosed_and_read_back() {
    let Some(bin) = server_bin() else {
        assert!(std::env::var_os("CHLOEDB_NETOPS_NEED_SERVER").is_none(), "no chloedb-server binary: build chloedb-server first, or set CHLOEDB_SERVER_BIN");
        eprintln!("skipped: no chloedb-server binary (build chloedb-server, or set CHLOEDB_SERVER_BIN)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let app = App::start(dir.path(), &bin);
    let api = Api::ready(&app.url).await;

    // The door: no token and a browser's cross-site request are both refused.
    let r = api.http.get(format!("{}/v1/status", api.url)).send().await.unwrap();
    assert_eq!(r.status(), 401, "no token");
    let r = api.http.get(format!("{}/v1/status", api.url)).bearer_auth(TOKEN).header("origin", "https://elsewhere.example").send().await.unwrap();
    assert_eq!(r.status(), 403, "a request with an Origin");
    let m = api.get("/app.json").await;
    assert_eq!(m["name"], "netops");
    let view = api.http.get(format!("{}/ui/view.js", api.url)).bearer_auth(TOKEN).send().await.unwrap().text().await.unwrap();
    assert!(view.contains("export default { render }"), "the view is a module the command center can load");
    // A client that has the view asks again with its ETag and is told it is the same, with no body.
    let r = api.http.get(format!("{}/ui/view.js", api.url)).bearer_auth(TOKEN).send().await.unwrap();
    let tag = r.headers().get("etag").expect("the view has an ETag").to_str().unwrap().to_string();
    let again = api.http.get(format!("{}/ui/view.js", api.url)).bearer_auth(TOKEN).header("if-none-match", &tag).send().await.unwrap();
    assert_eq!(again.status(), 304, "the view is not sent again");
    assert!(again.bytes().await.unwrap().is_empty());
    let other = api.http.get(format!("{}/ui/view.js", api.url)).bearer_auth(TOKEN).header("if-none-match", "\"0\"").send().await.unwrap();
    assert_eq!(other.status(), 200, "another version is sent whole");

    // The network it built, read the way the page reads it.
    let st = api.get("/v1/status").await;
    assert_eq!(st["network"]["devices"], 64);
    assert_eq!(st["network"]["sites"], 10);
    // The estate behind the backbone has outages of its own from the start; the backbone is calm.
    let calm = api.get("/v1/incidents").await["incidents"].as_array().unwrap().iter().filter(|i| i["status"] != "resolved" && i["category"] != "estate").count();
    assert_eq!(calm, 0, "a calm network opens no incident: {st}");
    let ov = api.get("/v1/overview").await;
    ran_sql(&ov, "overview");
    assert!(len(&ov["traffic"]) > 60 && len(&ov["services"]) == 12 && len(&ov["recent"]) > 0, "overview: {}", ov["at"]);
    let data = api.get("/v1/data").await;
    assert_eq!(len(&data["tables"]), 39);
    assert!(data["tables"].as_array().unwrap().iter().all(|t| t["rows"].as_i64().is_some()), "every table answers a count");
    // NET-21: each count is in the SQL the page shows.
    let counts = data["queries"].as_array().unwrap().iter().filter(|q| q["sql"].as_str().unwrap().starts_with("SELECT COUNT(*) AS n FROM netops_")).count();
    assert_eq!(counts, 39, "every count is traced: {}", len(&data["queries"]));

    // The estate behind the backbone, as the map reads it: the places, what
    // is broken and where, a window at a level of detail, and one object
    // with what it hangs from, what depends on it and what was done to it.
    let base = api.get("/v1/map/base").await;
    ran_sql(&base, "the map's places");
    let objects = base["objects"].as_i64().unwrap();
    assert!(objects > 2000 && len(&base["containers"]) > 20 && len(&base["trunks"]) > 5 && base["site_r"].as_f64().unwrap() > 0.0, "the estate: {} objects", objects);
    let hot = api.get("/v1/map/hot").await;
    let faults = hot["faults"].as_array().unwrap();
    assert!(!faults.is_empty(), "faults are kept going from the start");
    // A power plant hangs from nothing, so the walk upstream is asked of something that does.
    let f = ["access", "aggregation", "premises"].iter().find_map(|l| faults.iter().find(|f| f["layer"] == *l)).expect("a fault on something that hangs from the network");
    let (fid, metro, hub) = (f["id"].as_str().unwrap(), f["metro"].as_str().unwrap(), f["hub"].as_str().unwrap());
    for place in [f["region"].as_str().unwrap(), metro, hub] {
        assert!(hot["places"][place][f["state"].as_str().unwrap()].as_i64().unwrap() >= 1, "{fid} counts in {place}: {}", hot["places"][place]);
    }
    let (x, y) = (f["x"].as_f64().unwrap(), f["y"].as_f64().unwrap());
    let r = base["site_r"].as_f64().unwrap() * 3.0;
    let win = api.get(&format!("/v1/map/view?x0={}&y0={}&x1={}&y1={}&t=4", x - r, y - r, x + r, y + r)).await;
    assert!(win["nodes"].as_array().unwrap().iter().any(|n| n["id"] == fid) && len(&win["edges"]) > 0 && win["cut"] == false, "the window holds {fid}: {} objects", len(&win["nodes"]));
    let far = api.get(&format!("/v1/map/view?x0={}&y0={}&x1={}&y1={}&t=3", x + 5000.0, y, x + 5001.0, y + 1.0)).await;
    assert_eq!(len(&far["nodes"]), 0, "nothing out there");
    let node = api.get(&format!("/v1/map/node?id={fid}")).await;
    assert_eq!(node["engine"], "sql", "one server walks the graph in SQL");
    assert_eq!(node["cause"]["id"], fid, "what is wrong starts here: {}", node["cause"]);
    assert!(len(&node["upstream"]) >= 1 && node["upstream"][0]["hop"] == 1, "what it hangs from: {}", node["upstream"]);
    assert!(node["history"].as_array().unwrap().iter().any(|h| h["node"] == fid && h["event"] == "fault" && h["relation"] == "this object"), "its own fault is in its history");
    assert!(node["queries"].as_array().unwrap().iter().any(|q| q["sql"].as_str().unwrap().contains("REACHABLE(netops_estate_edges")), "walked over the edge table");
    // How such faults ended counts only the faults that read like this one, so its own cause comes first.
    let own = node["history"].as_array().unwrap().iter().find(|h| h["node"] == fid && h["event"] == "fault").unwrap()["cause"].clone();
    assert!(node["before"]["matched"].as_i64().unwrap_or(0) > 0 && node["before"]["ended"][0]["cause"] == own, "{fid} ({own}, {:?}) ended like: {}", node["before"]["words"], node["before"]["ended"]);
    if f["layer"] == "access" && f["state"] == "down" {
        // What is behind a dead access node says where its trouble comes from.
        let behind = api.get(&format!("/v1/map/view?x0={}&y0={}&x1={}&y1={}&t=4", x - r, y - r, x + r, y + r)).await;
        let child = behind["nodes"].as_array().unwrap().iter().find(|n| n["state"] == "impacted" && n["asite"] == f["asite"]).expect("something behind it").clone();
        let c = api.get(&format!("/v1/map/node?id={}", child["id"].as_str().unwrap())).await;
        assert!(c["cause"]["id"] == fid && c["cause"]["hop"] == 1, "the trouble is one hop up: {}", c["cause"]);
    }
    assert!(!api.http.get(format!("{}/v1/map/node?id=nothing", api.url)).bearer_auth(TOKEN).send().await.unwrap().status().is_success());

    // A search in a sentence: every condition it names in one statement.
    let up = node["upstream"].as_array().unwrap().iter().find(|u| u["layer"] == "core").or(node["upstream"].as_array().unwrap().last()).unwrap()["id"].as_str().unwrap().to_string();
    let ask = |q: String| { let api = &api; async move { let r = api.http.post(format!("{}/v1/map/ask", api.url)).json(&json!({"q": q})).bearer_auth(TOKEN).send().await.unwrap(); assert!(r.status().is_success(), "{q}: {}", r.status()); r.json::<Value>().await.unwrap() } };
    let found = ask(format!("everything behind {up} that is not ok")).await;
    assert!(found["understood"] == true && found["sql"].as_str().unwrap().contains("REACHABLE(") && found["rows"].as_array().unwrap().iter().any(|r| r["id"] == fid), "{fid} is behind {up} and broken: {} matches, {}", found["total"], found["read"]);
    assert!(found["places"][metro].as_i64().unwrap() >= 1, "counted where it is, for the map");
    let found = ask("everything changed in the last 2 months about an upgrade by operator m.rossi with latency above 1 ms".into()).await;
    let sql = found["sql"].as_str().unwrap();
    assert!(found["total"].as_i64().unwrap() > 0 && len(&found["read"]) == 2 && sql.contains("MATCH(text, :about)") && sql.contains("author = :author") && sql.contains("latency_ms > :latency_ms") && sql.contains("SELECT node FROM netops_estate_events"), "one statement: {sql}");
    let first = found["rows"][0]["id"].as_str().unwrap();
    assert!(found["why"][first]["author"] == "m.rossi" && found["why"][first]["text"].as_str().unwrap().to_lowercase().contains("upgrade"), "the change that matched: {}", found["why"][first]);
    assert_eq!(found["reader"], "rules", "no model is set in the test");
    let none = ask("how is the weather today".into()).await;
    assert!(none["understood"] == false && none["total"] == 0);
    assert!(len(&api.get("/v1/map/asks").await["asks"]) >= 2, "the searches are kept");
    // A GET reads and writes nothing: the history stays as it was, and no model is asked.
    let kept = api.get("/v1/data").await["tables"].as_array().unwrap().iter().find(|t| t["table"] == "netops_searches").map(|t| t["rows"].clone());
    let read = api.get(&format!("/v1/map/ask?q={}", "OLTs%20that%20are%20down")).await;
    assert!(read["understood"] == true && read["reader"] == "rules", "a GET is answered: {}", read["note"]);
    assert_eq!(api.get("/v1/data").await["tables"].as_array().unwrap().iter().find(|t| t["table"] == "netops_searches").map(|t| t["rows"].clone()), kept, "a GET kept no search");

    // The box completes from the tables: an id by PREFIX, the rest of a sentence, a filter's values, and the assistant's questions.
    let q = |s: &str| s.replace(' ', "%20").replace(':', "%3A");
    let prefix: String = fid.chars().take(fid.len().min(9)).collect();
    let sug = api.get(&format!("/v1/suggest?q={}", q(&prefix))).await;
    assert!(sug["items"].as_array().unwrap().iter().any(|i| i["kind"] == "object" && i["id"].as_str().unwrap().starts_with(&prefix)), "{prefix} completes to an object: {}", sug["items"]);
    assert!(sug["queries"].as_array().unwrap().iter().any(|x| x["sql"].as_str().unwrap().contains("PREFIX(id, :w)")), "by PREFIX");
    let sug = api.get(&format!("/v1/suggest?q={}", q("OLTs that are not "))).await;
    assert!(sug["items"].as_array().unwrap().iter().any(|i| i["text"] == "OLTs that are not ok"), "a state follows 'not': {}", sug["items"]);
    let sug = api.get(&format!("/v1/suggest?q={}", q("kind:ol"))).await;
    assert_eq!(sug["items"][0]["text"], "kind:olt", "a filter's values: {}", sug["items"]);
    let sug = api.get(&format!("/v1/suggest?q=&mode=ask&sel={fid}&prev={}", q("What is wrong with it, and why?"))).await;
    assert_eq!(sug["items"][0]["intent"], "changed", "after what is wrong, what changed: {}", sug["items"]);
    let det = api.get(&format!("/v1/map/detail?id={fid}")).await;
    assert!(len(&det["events"]) >= 1 && det["below"].is_array(), "the inspector's tabs: {}", det["events"]);
    let det = api.get("/v1/map/detail?id=cr1-lon").await;
    assert!(det["core"] == true && len(&det["metrics"]) > 0 && len(&det["links"]) > 0, "a backbone device has series");
    let dash = api.get("/v1/dash").await;
    assert!(len(&dash["links"]["top"]) > 0 && len(&dash["estate"]["heat"]) == 14, "the dashboards");
    for path in ["/v1/incidents", "/v1/alerts", "/v1/rules", "/v1/changes", "/v1/devices", "/v1/services", "/v1/flows", "/v1/agents", "/v1/devices/cr1-lon", "/v1/links/cr1-lon--cr1-nyc", "/v1/services/ip-transit", "/v1/devices/cr1-lon/blame"] {
        ran_sql(&api.get(path).await, path);
    }
    assert_eq!(len(&api.get("/v1/rules").await["rules"]), 16);

    // The topology read from the logs: two cables nobody wrote down, and two
    // inventory links that no device has ever named.
    api.post("/v1/topology/discover", json!({})).await;
    let topo = api.get("/v1/topology").await["compare"].clone();
    let pair = |v: &Value| format!("{}--{}", v["src"].as_str().unwrap(), v["dst"].as_str().unwrap());
    let mut extra: Vec<String> = topo["undocumented"].as_array().unwrap().iter().map(pair).collect();
    extra.sort();
    assert_eq!(extra, ["ag1-lon--ag2-lon", "br1-ams--br2-ams"], "undocumented: {topo}");
    let mut silent: Vec<String> = topo["silent"].as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap().to_string()).collect();
    silent.sort();
    assert_eq!(silent, ["ag1-par--br1-par", "cr1-sto--cr2-waw"], "silent: {topo}");
    assert_eq!(topo["confirmed"], 118);
    let ev = api.get("/v1/topology/evidence?a=ag1-lon&b=ag2-lon").await;
    assert!(len(&ev["lines"]) > 0 && len(&ev["inventory"]) == 0, "evidence: {ev}");
    let path = api.get("/v1/topology/path?from=lf1-fra&to=br1-nyc").await;
    assert!(len(&path["path"]) >= 4, "a path across the backbone: {path}");

    // Logs by substring (a piece of an interface name) and by words; one search over everything.
    let logs = api.get("/v1/logs?q=LLDP_NEIGHBOR&minutes=120").await;
    ran_sql(&logs, "logs");
    assert!(len(&logs["lines"]) > 0, "substring search");
    let found = api.get("/v1/search?q=bgp%20sessions%20dropped%20after%20a%20policy%20change").await;
    ran_sql(&found, "search");
    assert_eq!(found["runbooks"][0]["id"], "rb-bgp-rollback", "search: {}", found["runbooks"]);

    // A rule of one's own: tested, saved, listed, removed.
    let sql = "SELECT device AS entity, MAX(temp_c) AS v FROM netops_device_metrics WHERE ts >= :since GROUP BY device HAVING MAX(temp_c) > 200";
    let t = api.post("/v1/rules/test", json!({"sql": sql})).await;
    assert!(t["ok"] == true && t["count"] == 0, "rule test: {t}");
    let r = api.http.post(format!("{}/v1/rules", api.url)).bearer_auth(TOKEN).json(&json!({"name": "x", "sql": "DELETE FROM netops_devices"})).send().await.unwrap();
    assert_eq!(r.status(), 400, "a rule is a SELECT");
    let saved = api.post("/v1/rules", json!({"name": "Chassis on fire", "sql": sql, "severity": "major", "entity_kind": "device", "threshold": 200, "unit": "°C"})).await;
    assert_eq!(saved["id"], "chassis-on-fire");
    assert_eq!(len(&api.get("/v1/rules").await["rules"]), 17);
    api.post("/v1/rules/chassis-on-fire/delete", json!({})).await;

    // An outage behind the backbone reaches the shift: an access node with
    // objects behind it goes down (written straight to the server, as a feed
    // of the estate would), its rule fires, an incident opens on it and the
    // estate's agent says what is behind it. Diagnose and the lookup find it.
    let sql = |q: String| { let (http, url) = (api.http.clone(), app.strata.clone()); async move {
        let r = http.post(format!("{url}/v1/sql")).json(&json!({"sql": q})).send().await.unwrap();
        let status = r.status();
        let j: Value = r.json().await.unwrap();
        assert!(status.is_success(), "{j}");
        j
    } };
    let busy: Vec<String> = faults.iter().filter_map(|f| f["hub"].as_str().map(str::to_string)).collect();
    let cand = api.http.post(format!("{}/v1/query", app.strata)).json(&json!({"objects": true, "sql": "SELECT n.id AS id, MAX(n.hub) AS hub, COUNT(*) AS c FROM netops_estate_nodes n JOIN netops_estate_edges e ON e.src = n.id\nWHERE n.state = 'ok' AND n.layer = 'access'\nGROUP BY n.id ORDER BY c DESC LIMIT 50"})).send().await.unwrap().json::<Value>().await.unwrap();
    let olt = cand["rows"].as_array().unwrap().iter().find(|r| r["c"].as_i64().unwrap() >= 20 && !busy.iter().any(|h| r["hub"] == h.as_str())).unwrap_or_else(|| panic!("an access node with 20 behind it: {cand}"))["id"].as_str().unwrap().to_string();
    sql(format!("UPDATE netops_estate_nodes SET state = 'down', reason = 'Loss of signal on the PON uplink' WHERE id = '{olt}'")).await;
    let t0 = Instant::now();
    let out = loop {
        let incs = api.get("/v1/incidents").await;
        if let Some(i) = incs["incidents"].as_array().unwrap().iter().find(|i| i["status"] != "resolved" && i["root_entity"] == olt.as_str()) {
            break i.clone();
        }
        assert!(t0.elapsed() < Duration::from_secs(180), "an outage in the estate opened no incident: {}", api.get("/v1/alerts?status=firing").await["alerts"]);
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let id = out["id"].as_str().unwrap().to_string();
    let inc = api.get(&format!("/v1/incidents/{id}")).await;
    let i = &inc["incident"];
    assert!(i["root_kind"] == "estate" && i["category"] == "estate" && i["title"].as_str().unwrap().contains(&olt) && i["root_cause"].as_str().unwrap().contains("Loss of signal"), "the estate's incident: {i}");
    assert!(inc["steps"].as_array().unwrap().iter().any(|s| s["sql_text"].as_str().unwrap_or("").contains("netops_estate")), "the estate's agent kept its queries: {}", inc["steps"]);
    let board = api.get(&format!("/v1/incidents/{id}/panels")).await;
    assert!(len(&board["panels"]) == 3 && board["panels"].as_array().unwrap().iter().all(|p| p["error"] == ""), "the estate's board reads the estate: {}", board["panels"]);
    let alerts = api.get("/v1/alerts?status=firing").await;
    assert!(alerts["alerts"].as_array().unwrap().iter().any(|a| a["entity"] == olt.as_str() && a["rule"] == "estate-outage" && a["severity"] == "major" && a["incident"] == id.as_str()), "its alert");
    let wh = api.get("/v1/where").await;
    assert!(wh["estate"].as_array().unwrap().iter().any(|a| a["entity"] == olt.as_str() && a["incident"] == id.as_str()), "Diagnose names it: {}", wh["estate"]);
    let found = api.get(&format!("/v1/lookup?q={olt}")).await;
    assert!(found["hits"][0]["id"] == olt.as_str() && found["hits"][0]["exact"] == true && found["hits"][0]["health"]["incident"] == id.as_str(), "the lookup finds it by its id: {found}");
    sql(format!("UPDATE netops_estate_nodes SET state = 'ok', reason = '' WHERE id = '{olt}'")).await;
    api.post(&format!("/v1/incidents/{id}/close"), json!({"who": "test", "runbook": "rb-estate-outage", "grade": 3})).await;

    // The agents that run on demand.
    let cap = api.post("/v1/agents/capacity/run", json!({})).await;
    // What it finds depends on the hour (the network is quiet at night), so only that it looked.
    assert!(cap["status"] == "done" && cap["result"]["findings"].is_array() && len(&cap["step_list"]) > 0, "capacity: {cap}");
    let comp = api.post("/v1/agents/compliance/run", json!({})).await;
    assert!(len(&comp["result"]["exposed"]) > 0 && len(&comp["result"]["drift"]) > 0, "compliance: {comp}");
    let chg = api.get("/v1/changes").await;
    let next = chg["changes"].as_array().unwrap().iter().find(|c| c["status"] == "approved").expect("a planned change")["id"].as_str().unwrap().to_string();
    let risk = api.post(&format!("/v1/changes/{next}/risk"), json!({})).await;
    assert!(risk["result"]["score"].as_i64().is_some_and(|s| (0..=100).contains(&s)), "change risk: {risk}");

    // Break it: each fault in turn, and the root-cause agent has to name what
    // the simulator did, from the tables alone.
    let scenarios: Vec<String> = st["sim"]["scenarios"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(scenarios.len(), 6);
    let mut fibre = String::new();
    for sc in &scenarios {
        // A seed fixes where each fault lands, so the six do not share a device.
        api.post("/v1/sim/inject", json!({"scenario": sc, "seed": 1})).await;
        let ov = api.get("/v1/overview").await;
        // The estate's own outages come and go beside it; the fault injected is on the backbone.
        let open: Vec<&Value> = ov["open"].as_array().unwrap().iter().filter(|i| i["category"] != "estate").collect();
        assert_eq!(open.len(), 1, "{sc}: one incident for one fault, not one per alert: {open:?}");
        let id = open[0]["id"].as_str().unwrap().to_string();
        let inc = api.get(&format!("/v1/incidents/{id}")).await;
        ran_sql(&inc, "incident");
        let key = api.get(&format!("/v1/incidents/{id}/key")).await["key"].clone();
        assert_eq!(key["scenario"], sc.as_str());
        let trail: String = inc["steps"].as_array().unwrap().iter().map(|s| format!("\n  {} {}: {}", s["n"], s["title"].as_str().unwrap_or(""), s["detail"].as_str().unwrap_or(""))).collect();
        assert_eq!(inc["incident"]["root_entity"], key["truth_entity"], "{sc}: the agent named {} ({}); the fault was on {}. Its steps:{trail}", inc["incident"]["root_entity"], inc["incident"]["root_cause"], key["truth_entity"]);
        assert_eq!(inc["incident"]["category"], sc.as_str());
        assert!(len(&inc["alerts"]) > 0 && len(&inc["timeline"]) >= 3 && len(&inc["blast"]) > 0, "{sc}: {}", inc["incident"]);
        // The run kept its steps with their SQL, and a receipt of what it packed.
        let steps = inc["steps"].as_array().unwrap();
        assert!(steps.iter().any(|s| s["tool"] == "graph") && steps.iter().any(|s| s["tool"] == "pack") && steps.iter().filter(|s| s["tool"] != "reason").all(|s| s["sql_text"].as_str().is_some_and(|q| !q.is_empty())), "{sc}: steps {steps:?}");
        assert!(steps.iter().all(|s| !s["detail"].as_str().unwrap_or("").starts_with("Refused")), "{sc}: a step was refused: {steps:?}");
        assert!(inc["receipts"].as_array().unwrap().iter().any(|r| r["decision"] == "take"), "{sc}: an empty brief");
        assert!(!inc["runbook"].is_null() && len(&inc["actions"]) > 0, "{sc}: a runbook and something to do");
        let run = inc["runs"].as_array().unwrap().iter().rfind(|r| r["agent"] == "root-cause").unwrap().clone();
        assert_eq!(run["verdict"], "correct", "{sc}: {run}");
        ran_sql(&api.get(&format!("/v1/incidents/{id}/replay")).await, "replay");

        // While it is broken: the stack by place says where, lowest layer first,
        // and one investigation asks the graph, the logs, the history and the past.
        if sc == "fiber_cut" {
            let wh = api.get("/v1/where").await;
            ran_sql(&wh, "where");
            assert!(wh["start"]["layer"] == "optics" && wh["start"]["col"] == "backbone" && wh["start"]["first"]["kind"] == "link", "a fibre cut begins at the optics of a backbone link: {}", wh["start"]);
            assert!(wh["cells"]["links"]["backbone"]["state"] == "bad" && wh["cells"]["power"]["lon"]["state"] == "ok", "the matrix: {}", wh["cells"]["links"]);
            let link = wh["start"]["first"]["id"].as_str().unwrap().to_string();
            let inv = api.get(&format!("/v1/investigate?kind=link&id={link}")).await;
            ran_sql(&inv, "investigate");
            let lens = |id: &str| inv["lenses"].as_array().unwrap().iter().find(|l| l["id"] == id).unwrap_or_else(|| panic!("no {id} lens"))["data"].clone();
            assert_eq!(len(&inv["lenses"]), 5);
            assert!(lens("graph")["suspects"].as_array().unwrap().iter().take(4).any(|s| s["this"] == true) && len(&lens("graph")["services"]) > 0, "the graph: {}", lens("graph"));
            assert!(len(&lens("logs")["lines"]) > 0, "its interface is named in the logs: {}", lens("logs")["needle"]);
            assert!(len(&lens("before")["incidents"]) > 0 && len(&lens("time")["series"]) > 30, "the past and the last hour");
            let (a, b) = link.split_once("--").unwrap();
            let tr = api.get(&format!("/v1/trace?from={a}&to={b}")).await;
            assert!(tr["state"] == "bad" && tr["verdict"].as_str().unwrap().starts_with("Broken at hop 1"), "the trace marks the hop: {}", tr["verdict"]);
        }
        if sc == "fiber_cut" {
            fibre = id.clone();
            // Approve the reroute: it is carried out, written to the timeline, and no longer waits.
            let act = inc["actions"].as_array().unwrap().iter().find(|a| a["kind"] == "reroute").expect("a reroute is proposed").clone();
            let done = api.post(&format!("/v1/actions/{}/approve", act["id"].as_str().unwrap().replace(':', "%3A")), json!({"who": "test"})).await;
            assert_eq!(done["status"], "executed", "approve: {done}");
            let after = api.get(&format!("/v1/incidents/{id}")).await;
            assert!(after["timeline"].as_array().unwrap().iter().any(|t| t["kind"] == "action"), "the action is on the timeline");
            assert!(api.get("/v1/actions").await["actions"].as_array().unwrap().iter().all(|a| a["id"] != act["id"]), "it no longer waits");
        }
        // Repair it and close the incident with the runbook that worked.
        api.post("/v1/sim/end", json!({})).await;
        let rb = inc["runbook"]["id"].as_str().unwrap();
        api.post(&format!("/v1/incidents/{id}/close"), json!({"who": "test", "runbook": rb, "grade": 3})).await;
        let closed = api.get(&format!("/v1/incidents/{id}")).await;
        assert_eq!(closed["incident"]["status"], "resolved");
        assert!(!closed["incident"]["postmortem"].as_str().unwrap_or("").is_empty(), "{sc}: a postmortem draft");
        assert!(len(&closed["judgments"]) > 0, "{sc}: the judgment is kept");
        assert_eq!(api.get("/v1/overview").await["open"].as_array().unwrap().iter().filter(|i| i["category"] != "estate").count(), 0);
    }
    let verdicts = api.get("/v1/agents").await["verdicts"].clone();
    assert_eq!(verdicts, json!([{"n": 6, "verdict": "correct"}]), "six faults, six right answers");

    // An alert from outside (Alertmanager's shape) lands in the same table, and clears the same way.
    let outside = |status: &str| json!({"alerts": [{"status": status, "labels": {"alertname": "FanFailure", "instance": "cr1-lon:9100", "severity": "warning"}, "annotations": {"summary": "Fan tray 2 failed"}}]});
    assert_eq!(api.post("/v1/alerts/webhook", outside("firing")).await["received"], 1);
    let firing = api.get("/v1/alerts?status=firing").await;
    assert!(firing["alerts"].as_array().unwrap().iter().any(|a| a["rule_name"] == "FanFailure" && a["entity"] == "cr1-lon" && a["severity"] == "major"), "the outside alert: {}", firing["alerts"]);
    api.post("/v1/alerts/webhook", outside("resolved")).await;
    assert!(api.get("/v1/alerts?status=firing").await["alerts"].as_array().unwrap().iter().all(|a| a["rule_name"] != "FanFailure"), "and it clears");

    // The shift between incidents. The handover and the list of what waits.
    let shift = api.get("/v1/shift").await;
    ran_sql(&shift, "shift");
    assert!(len(&shift["briefing"]) >= 3 && len(&shift["work"]) > 0 && len(&shift["notes"]) == 3, "shift: {}", shift["counts"]);
    // NET-20: the tabs' badges are what the worklist holds (read again if
    // a minute counted them between the two reads).
    for tries in 1.. {
        let shift = api.get("/v1/shift").await;
        let st = api.get("/v1/status").await;
        let work = shift["work"].as_array().unwrap();
        let want = (Some(work.iter().filter(|w| w["urgency"] == 1).count() as i64), Some(work.iter().filter(|w| w["kind"] == "maintenance").count() as i64));
        if (st["waiting"].as_i64(), st["notices"].as_i64()) == want {
            break;
        }
        assert!(tries < 3, "the badges of My shift and Carriers: {} and {}, the worklist {want:?}", st["waiting"], st["notices"]);
    }
    let note = api.post("/v1/shift/notes", json!({"who": "test", "body": "Do not touch ag1-ams tonight"})).await;
    assert!(api.get("/v1/shift").await["notes"].as_array().unwrap().iter().any(|n| n["id"] == note["id"]), "a note left for the next shift is read back");

    // A thing looked up by what a ticket calls it: an address inside a
    // customer's prefix, a circuit id, a device.
    let cust = api.get("/v1/customers").await["customers"][0].clone();
    let ip = cust["prefix"].as_str().unwrap().replace(".0/24", ".9");
    let hit = api.get(&format!("/v1/lookup?q={ip}")).await;
    assert!(hit["hits"].as_array().unwrap().iter().any(|h| h["kind"] == "customer" && h["id"] == cust["id"]), "{ip} belongs to {}: {}", cust["id"], hit["hits"]);
    assert!(len(&api.get("/v1/lookup?q=cr1-lon").await["hits"]) > 0);
    // "Is it the network?" for that customer: what was checked, and a reply.
    let check = api.get(&format!("/v1/customers/{}", cust["id"].as_str().unwrap())).await;
    ran_sql(&check, "customer check");
    assert!(["clear", "now", "earlier", "change"].contains(&check["verdict"].as_str().unwrap()) && len(&check["checked"]) >= 4 && check["reply"].as_str().unwrap().contains("Hello"), "check: {}", check["headline"]);

    // An alert gets an owner; a silence keeps a device from paging and is ended.
    assert_eq!(api.post("/v1/alerts/webhook", outside("firing")).await["received"], 1);
    let firing = api.get("/v1/alerts?status=firing").await;
    let fan = firing["alerts"].as_array().unwrap().iter().find(|a| a["rule_name"] == "FanFailure").expect("the outside alert").clone();
    let fan_id = fan["id"].as_str().unwrap().replace(':', "%3A");
    api.post(&format!("/v1/alerts/{fan_id}/ack"), json!({"who": "test"})).await;
    let firing = api.get("/v1/alerts?status=firing").await;
    assert!(firing["alerts"].as_array().unwrap().iter().any(|a| a["rule_name"] == "FanFailure" && a["acked_by"] == "test"), "acknowledged: {}", firing["alerts"]);
    api.post("/v1/alerts/webhook", outside("resolved")).await;
    let sil = api.post("/v1/silences", json!({"kind": "device", "matcher": "cr1-lon", "reason": "linecard swap", "hours": 1, "who": "test"})).await;
    let sid = sil["id"].as_str().or(sil["silence"]["id"].as_str()).expect("the silence's id").to_string();
    let list = api.get("/v1/silences").await;
    assert!(list["silences"].as_array().unwrap().iter().any(|x| x["id"] == sid.as_str() && x["status"] == "active"), "silences: {}", list["silences"]);
    api.post(&format!("/v1/silences/{sid}/end"), json!({})).await;
    assert!(api.get("/v1/silences").await["silences"].as_array().unwrap().iter().any(|x| x["id"] == sid.as_str() && x["status"] != "active"), "ended");

    // A change carried out: started (its devices go quiet), compared with
    // the network before it, completed.
    let changes = api.get("/v1/changes").await;
    let chg = changes["changes"].as_array().unwrap().iter().find(|c| c["status"] == "approved" || c["status"] == "scheduled").expect("a change waiting to be done")["id"].as_str().unwrap().to_string();
    api.post(&format!("/v1/changes/{chg}/start"), json!({"who": "test"})).await;
    let c = api.get(&format!("/v1/changes/{chg}")).await;
    assert_eq!(c["change"]["status"], "in_progress");
    assert!(api.get("/v1/silences").await["silences"].as_array().unwrap().iter().any(|x| x["source"] == format!("change:{chg}").as_str() && x["status"] == "active"), "a change in progress silences its devices");
    api.post(&format!("/v1/changes/{chg}/verify"), json!({"who": "test"})).await;
    let c = api.get(&format!("/v1/changes/{chg}")).await;
    assert!(len(&c["checks"]["checks"]) >= 3 && c["checks"]["ok"] == true, "nothing changed on a calm network: {}", c["checks"]);
    api.post(&format!("/v1/changes/{chg}/complete"), json!({"who": "test"})).await;
    assert_eq!(api.get(&format!("/v1/changes/{chg}")).await["change"]["status"], "completed");

    // Carriers' notices: the two that take both planes of one path collide,
    // and a pasted notice is read into its circuit and window.
    let mnt = api.get("/v1/maintenance").await;
    ran_sql(&mnt, "maintenance");
    assert!(len(&mnt["notices"]) >= 5 && mnt["conflicts"].as_object().unwrap().values().any(|c| c.as_array().unwrap().iter().any(|x| x["severity"] == "critical")), "conflicts: {}", mnt["conflicts"]);
    let circuit = mnt["notices"][0]["circuit"].as_str().unwrap();
    let pasted = api.post("/v1/maintenance/parse", json!({"text": format!("Subject: Planned work\n\nReference: T-1\nService ID: {circuit}\nStart: 2031-01-02 01:00 (UTC)\nEnd: 2031-01-02 05:00 (UTC)\nExpected impact: Outage of up to 2 hours\n")})).await;
    assert!(pasted["circuit"] == circuit && pasted["window_start"] == "2031-01-02T01:00:00Z" && pasted["impact"] == "outage", "read from the text: {pasted}");
    let planned = api.post(&format!("/v1/maintenance/{}/plan", pasted["id"].as_str().unwrap()), json!({"who": "test"})).await;
    assert!(planned["status"] == "planned" && planned["change_id"].as_str().is_some_and(|c| c.starts_with("CHG-")), "planned: {planned}");

    // The incident's board: its cause's panels, one asked for in a sentence,
    // one refused, one removed.
    let panels = || async { api.get("/v1/data").await["tables"].as_array().unwrap().iter().find(|t| t["table"] == "netops_panels").map(|t| t["rows"].clone()) };
    let before = panels().await;
    let board = api.get(&format!("/v1/incidents/{fibre}/panels")).await;
    ran_sql(&board, "board");
    assert_eq!(panels().await, before, "looking at the board writes nothing");
    let titles: Vec<&str> = board["panels"].as_array().unwrap().iter().map(|p| p["title"].as_str().unwrap()).collect();
    assert!(titles.len() >= 5 && titles.iter().any(|t| t.starts_with("Packet loss")) && titles.iter().any(|t| t.starts_with("Link state")), "a fibre cut's board: {titles:?}");
    assert!(board["panels"].as_array().unwrap().iter().all(|p| p["error"] == ""), "every panel's query runs: {}", board["panels"]);
    let added = api.post(&format!("/v1/incidents/{fibre}/panels"), json!({"ask": "cpu on cr1-lon last 2 hours", "who": "test"})).await;
    assert_eq!(added["understood"], "Control-plane CPU on cr1-lon, last 2 h");
    assert!(added["rows"].as_i64().unwrap() > 60);
    for no in ["make me a sandwich", "DELETE FROM netops_panels", "SELECT 1; DROP TABLE netops_panels"] {
        let r = api.http.post(format!("{}/v1/incidents/{fibre}/panels", api.url)).bearer_auth(TOKEN).json(&json!({"ask": no})).send().await.unwrap();
        assert_eq!(r.status(), 400, "{no}");
    }
    let board = api.get(&format!("/v1/incidents/{fibre}/panels")).await;
    assert_eq!(board["panels"][0]["title"], "Control-plane CPU on cr1-lon, last 2 h", "a new panel goes to the top");
    let pid = board["panels"][0]["id"].as_str().unwrap().replace(':', "%3A");
    api.post(&format!("/v1/incidents/{fibre}/panels/{pid}/remove"), json!({"who": "test"})).await;
    let board = api.get(&format!("/v1/incidents/{fibre}/panels")).await;
    assert!(board["removed"] == 1 && board["panels"].as_array().unwrap().iter().all(|p| p["source"] != "person"), "removed, and the template did not come back twice: {}", len(&board["panels"]));

    // The same tables from further away.
    let lead = api.get("/v1/leadership?days=30").await;
    ran_sql(&lead, "leadership");
    assert!(len(&lead["services"]) == 12 && lead["incidents"]["count"].as_i64().unwrap() >= 6 && len(&lead["carriers"]) > 3 && lead["report"].as_str().unwrap().contains("Incidents."), "leadership: {}", lead["incidents"]);
    // A rate measured over the telemetry kept and incident minutes over the period are said with their windows.
    let objectives = lead["report"].as_str().unwrap().lines().find(|l| l.starts_with("Objectives.")).unwrap_or("").to_string();
    assert!(objectives.contains(" h of telemetry") && objectives.contains("Over the 30 days of incidents"), "{objectives}");

    // Stop, start on the same folder: nothing is built again, and the
    // incidents, the judgments and the simulator's place are all still there.
    drop(api);
    app.stop();
    let t0 = Instant::now();
    let app = App::start(dir.path(), &bin);
    let api = Api::ready(&app.url).await;
    assert!(t0.elapsed() < Duration::from_secs(60), "a restart reads what is there");
    let inc = api.get(&format!("/v1/incidents/{fibre}")).await;
    assert_eq!(inc["incident"]["status"], "resolved");
    assert_eq!(inc["incident"]["category"], "fiber_cut");
    assert_eq!(api.get("/v1/agents").await["verdicts"], json!([{"n": 6, "verdict": "correct"}]));
    drop(api);
    app.stop();

    // The simulator off: the network is read from the inventory tables, and
    // the app writes no telemetry. Readings that arrive from elsewhere (here
    // a few rows written straight to the server, as a feed would) are judged
    // by the same rules, grouped and diagnosed.
    let app = App::start_with(dir.path(), &bin, &["--no-simulate"]);
    let api = Api::ready(&app.url).await;
    let st = api.get("/v1/status").await;
    assert!(st["sim"]["simulate"] == false && st["network"]["devices"] == 64 && st["network"]["sites"] == 10, "the network from the tables: {}", st["network"]);
    let r = api.http.post(format!("{}/v1/sim/inject", api.url)).bearer_auth(TOKEN).json(&json!({"scenario": "fiber_cut"})).send().await.unwrap();
    assert!(!r.status().is_success(), "nothing to inject into a real network");
    // An outage the simulator left in the estate's tables may still be open: only the backbone's count here.
    let before = api.get("/v1/incidents").await["incidents"].as_array().unwrap().iter().filter(|i| i["status"] != "resolved" && i["category"] != "estate").count();
    assert_eq!(before, 0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 / 60 * 60;
    let stamp = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let ops: Vec<Value> = (-4..6).map(|k| now + k * 60).map(|t| json!({"op": "upsert", "table": "netops_link_metrics", "doc": {"id": format!("feed:{t}"), "ts": stamp(t), "link": "cr1-par--cr1-mad",
        "util": 0.41, "in_bps": 4.1e10, "out_bps": 3.9e10, "errors": 1400, "discards": 30, "loss": 0.35, "latency_ms": 19.5, "rx_dbm": -14.2, "up": 1}})).collect();
    let r = api.http.post(format!("{}/v1/write", app.strata)).json(&json!({"ops": ops})).send().await.unwrap();
    assert!(r.status().is_success(), "the feed's write: {}", r.text().await.unwrap());
    // A notice as a mail connector leaves it: the text and a status only.
    let had = api.get("/v1/maintenance").await["notices"].as_array().unwrap().len();
    let circuits = api.get("/v1/maintenance").await["circuits"].clone();
    let circuit = circuits.as_array().unwrap().iter().find_map(|c| c["id"].as_str().or(c["circuit"].as_str())).expect("a circuit").to_string();
    let raw = format!("Subject: Planned work\nCircuit: {circuit}\nWindow: 2031-01-05 01:00 to 2031-01-05 03:00 (UTC)\nImpact: outage of up to 30 minutes");
    let op = json!({"op": "upsert", "table": "netops_maintenance", "doc": {"id": "MAIL-1", "provider": "", "subject": "Planned work", "received": stamp(now), "raw": raw, "status": "new"}});
    let r = api.http.post(format!("{}/v1/write", app.strata)).json(&json!({"ops": [op]})).send().await.unwrap();
    assert!(r.status().is_success(), "the mail's write: {}", r.text().await.unwrap());
    let t0 = Instant::now();
    let found = loop {
        let incs = api.get("/v1/incidents").await;
        if let Some(i) = incs["incidents"].as_array().unwrap().iter().find(|i| i["status"] != "resolved" && i["category"] == "optic_degrade") {
            break i.clone();
        }
        assert!(t0.elapsed() < Duration::from_secs(240), "readings from outside opened no incident: {}", api.get("/v1/alerts?status=firing").await["alerts"]);
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    assert!(found["title"].as_str().unwrap().contains("cr1-mad") || found["title"].as_str().unwrap().contains("cr1-par"), "the incident: {found}");
    let notices = api.get("/v1/maintenance").await;
    let mail = notices["notices"].as_array().unwrap().iter().find(|n| n["id"] == "MAIL-1").expect("the mail's notice").clone();
    assert!(mail["circuit"] == circuit.as_str() && !mail["link"].as_str().unwrap_or("").is_empty() && mail["window_start"].as_str().unwrap().starts_with("2031-01-05T01:00"), "the notice was read: {mail}");
    assert_eq!(notices["notices"].as_array().unwrap().len(), had + 1, "nothing invented for a real network");
    let data = api.get("/v1/data").await;
    assert!(data["sources"].as_array().unwrap().iter().any(|s| s["table_name"] == "netops_link_metrics" && s["status"] == "live"), "the source is seen arriving: {}", data["sources"]);
    app.stop();
}

/// A cluster on this machine: shards and a coordinator over them, each a
/// chloedb-server, stopped when dropped.
struct Cluster {
    kids: Vec<Child>,
    url: String,
}

impl Cluster {
    async fn start(dir: &std::path::Path, bin: &std::path::Path, shards: usize) -> Cluster {
        let mut kids = Vec::new();
        let mut grpcs = Vec::new();
        let http = reqwest::Client::new();
        let spawn = |args: Vec<String>, name: &str| {
            let log = std::fs::File::create(dir.join(format!("{name}.log"))).unwrap();
            Command::new(bin).args(args).env("CHLOEDB_START_APPS", "off").stdout(log.try_clone().unwrap()).stderr(log).spawn().expect("chloedb-server")
        };
        let up = |url: String| {
            let http = http.clone();
            async move {
                let t0 = Instant::now();
                while !http.get(format!("{url}/v1/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
                    assert!(t0.elapsed() < Duration::from_secs(60), "{url} did not start");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        };
        let mut urls = Vec::new();
        for i in 0..shards {
            let (h, g) = (free_port(), free_port());
            let db = dir.join(format!("shard-{i}.strata")).to_string_lossy().into_owned();
            kids.push(spawn(vec!["--db".into(), db, "--seq-no".into(), "--listen".into(), format!("127.0.0.1:{h}"), "--grpc".into(), format!("127.0.0.1:{g}")], &format!("shard-{i}")));
            grpcs.push(format!("http://127.0.0.1:{g}"));
            urls.push(format!("http://127.0.0.1:{h}"));
        }
        for u in urls {
            up(u).await;
        }
        let c = free_port();
        kids.push(spawn(vec!["--shards".into(), grpcs.join(","), "--listen".into(), format!("127.0.0.1:{c}"), "--grpc".into(), "off".into()], "coordinator"));
        let url = format!("http://127.0.0.1:{c}");
        up(url.clone()).await;
        Cluster { kids, url }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for k in self.kids.iter_mut().rev() {
            let _ = k.kill();
            let _ = k.wait();
        }
    }
}

/// The objects within `hops` of `from` along the edges, downstream, as
/// the app walks them: through nothing of the backbone but where it starts.
fn walk(edges: &[(String, String)], core: &std::collections::HashSet<String>, from: &str, hops: usize) -> Vec<String> {
    let mut down: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (a, b) in edges {
        down.entry(a.as_str()).or_default().push(b.as_str());
    }
    let mut seen = std::collections::HashSet::from([from.to_string()]);
    let mut queue = std::collections::VecDeque::from([(from.to_string(), 0)]);
    let mut out = Vec::new();
    while let Some((i, d)) = queue.pop_front() {
        if d == hops || (i != from && core.contains(&i)) {
            continue;
        }
        for j in down.get(i.as_str()).into_iter().flatten() {
            if seen.insert(j.to_string()) {
                out.push(j.to_string());
                queue.push_back((j.to_string(), d + 1));
            }
        }
    }
    out
}

/// No graph statement, none refused and none longer than a bound: what a
/// coordinator is sent while the app walks the estate itself (LIV-16, NET-7).
fn bounded(j: &Value, what: &str) {
    for q in j["queries"].as_array().unwrap_or_else(|| panic!("{what}: no queries listed")) {
        let sql = q["sql"].as_str().unwrap_or("");
        assert!(q["note"].as_str().unwrap_or("").is_empty(), "{what}: refused: {q}");
        assert!(!sql.contains("REACHABLE(") && !sql.contains("ppr(") && !sql.contains("PATH("), "{what}: a graph statement went to the coordinator: {}", &sql[..sql.len().min(300)]);
        assert!(sql.len() <= 20_000, "{what}: a statement of {} characters: {}", sql.len(), &sql[..300]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_estate_walked_through_a_coordinator() {
    let Some(bin) = server_bin() else {
        assert!(std::env::var_os("CHLOEDB_NETOPS_NEED_SERVER").is_none(), "no chloedb-server binary: build chloedb-server first, or set CHLOEDB_SERVER_BIN");
        eprintln!("skipped: no chloedb-server binary (build chloedb-server, or set CHLOEDB_SERVER_BIN)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cluster = Cluster::start(dir.path(), &bin, 2).await;
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_chloedb-netops"))
        .args(["serve", "--strata", &cluster.url, "--listen", &format!("127.0.0.1:{port}"), "--token", TOKEN])
        .args(["--fault-every", "0", "--history-hours", "1", "--estate", "16000", "--rewrite-model", ""])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("chloedb-netops");
    let app = App { child, url: format!("http://127.0.0.1:{port}"), strata: cluster.url.clone() };
    let api = Api::ready(&app.url).await;
    // It asked /v1/health once, and walks in the app from the start.
    assert_eq!(api.get("/v1/status").await["graph"], "app", "a coordinator is known as one");

    // The truth, read with plain queries: the edges and every object's state.
    let sql = |s: String, p: Value| {
        let (http, url) = (api.http.clone(), cluster.url.clone());
        async move {
            let j: Value = http.post(format!("{url}/v1/query")).json(&json!({"sql": s, "params": p, "objects": true})).send().await.unwrap().json().await.unwrap();
            j["rows"].as_array().unwrap_or_else(|| panic!("{s}: {j}")).clone()
        }
    };
    // Every row of a table, a scroll's page (at most 10,000 rows) at a time.
    let every = |table: &'static str, cols: &'static str| {
        let (http, url) = (api.http.clone(), cluster.url.clone());
        async move {
            let mut out = Vec::new();
            let names: Vec<String> = format!("id, {cols}").split(", ").map(str::to_string).collect();
            let mut body = json!({"sql": format!("SELECT id, {cols} FROM {table}"), "page_size": 10000});
            loop {
                let j: Value = http.post(format!("{url}/v1/query")).json(&body).send().await.unwrap().json().await.unwrap();
                // A row is a list of the columns, in the order asked.
                out.extend(j["rows"].as_array().unwrap_or_else(|| panic!("{table}: {j}")).iter().map(|r| Value::Object(names.iter().cloned().zip(r.as_array().unwrap().iter().cloned()).collect())));
                match j["cursor"].as_str() {
                    Some(c) => body = json!({"cursor": c}),
                    None => return out,
                }
            }
        }
    };
    let edges: Vec<(String, String)> = every("netops_estate_edges", "src, dst").await.iter().map(|r| (r["src"].as_str().unwrap().to_string(), r["dst"].as_str().unwrap().to_string())).collect();
    let n_edges = sql("SELECT COUNT(*) AS n FROM netops_estate_edges".into(), json!({})).await[0]["n"].as_u64().unwrap() as usize;
    assert_eq!(edges.len(), n_edges, "every edge read");
    let truth = || async {
        let rows = every("netops_estate_nodes", "layer, state").await;
        rows.iter().map(|r| (r["id"].as_str().unwrap().to_string(), (r["layer"].as_str().unwrap().to_string(), r["state"].as_str().unwrap().to_string()))).collect::<std::collections::HashMap<_, _>>()
    };
    let nodes = truth().await;
    let core: std::collections::HashSet<String> = nodes.iter().filter(|(_, (l, _))| l == "core").map(|(i, _)| i.clone()).collect();

    // A hub router: three things behind it are made to fail.
    let hub = "hr1-h01-ams";
    let below: Vec<String> = walk(&edges, &core, hub, 6);
    assert!(below.len() > 20, "{hub} has things behind it: {}", below.len());
    let broken: Vec<&String> = below.iter().filter(|i| nodes[*i].0 == "access").take(3).collect();
    let list = broken.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", ");
    let r = api.http.post(format!("{}/v1/sql", cluster.url)).json(&json!({"sql": format!("UPDATE netops_estate_nodes SET state = 'down', reason = 'test' WHERE id IN ({list})")})).send().await.unwrap();
    assert!(r.status().is_success(), "the update: {}", r.text().await.unwrap());
    let not_ok = |nodes: &std::collections::HashMap<String, (String, String)>, set: &[String]| set.iter().filter(|i| nodes[*i].1 != "ok" && nodes[*i].0 != "container").count() as i64;

    // NET-22: what depends on it, counted by its real states.
    let before = not_ok(&truth().await, &below);
    let node = api.get(&format!("/v1/map/node?id={hub}")).await;
    let after = not_ok(&truth().await, &below);
    bounded(&node, "the map's object");
    assert_eq!(node["engine"], "app");
    assert_eq!(node["downstream"]["total"].as_i64().unwrap(), below.len() as i64, "what depends on {hub}");
    let seen = node["downstream"]["not_ok"].as_i64().unwrap();
    assert!(seen >= 3 && (seen == before || seen == after), "of them not ok: {seen}, the tables say {before} then {after}: {}", node["downstream"]["rows"]);
    assert!(node["near"].as_array().unwrap().iter().any(|n| broken.iter().any(|b| n["id"] == b.as_str())), "trouble near it: {}", node["near"]);
    let detail = api.get(&format!("/v1/map/detail?id={hub}")).await;
    bounded(&detail, "the inspector");
    assert_eq!(len(&detail["below"]), below.len().min(4000), "what the map lights");

    let ask = |q: String| { let api = &api; async move { let r = api.http.post(format!("{}/v1/map/ask", api.url)).json(&json!({"q": q})).bearer_auth(TOKEN).send().await.unwrap(); assert!(r.status().is_success(), "{q}: {}", r.status()); r.json::<Value>().await.unwrap() } };
    // A small walk, read by key.
    let before = not_ok(&truth().await, &below);
    let found = ask(format!("everything behind {hub} that is not ok")).await;
    let after = not_ok(&truth().await, &below);
    bounded(&found, "a small walk");
    let total = found["total"].as_i64().unwrap();
    assert!(total >= 3 && (total == before || total == after), "behind {hub} and not ok: {total}, the tables say {before} then {after}");
    assert!(found["rows"].as_array().unwrap().iter().any(|r| broken.iter().any(|b| r["id"] == b.as_str())), "the broken ones are found");
    assert!(found["queries"].as_array().unwrap().iter().any(|q| q["what"].as_str().unwrap().contains("read by key")), "read by key: {}", found["queries"]);

    // A large walk, everything a metro's aggregation router carries: read
    // by the places that hold it, with no list of thousands of keys.
    let ag = "ag1-ams";
    let behind: Vec<String> = walk(&edges, &core, ag, 6).into_iter().filter(|i| nodes[i].0 != "container").collect();
    assert!(behind.len() > 1600, "a walk larger than what is read by key: {}", behind.len());
    let found = ask(format!("everything behind {ag}")).await;
    bounded(&found, "a large walk");
    assert_eq!(found["total"].as_i64().unwrap(), behind.len() as i64, "everything behind {ag}");
    assert!(found["queries"].as_array().unwrap().iter().any(|q| q["what"].as_str().unwrap().contains("places that hold the walk")), "read by place: {}", found["queries"]);
    let rows = found["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 300, "the first 300, in full");
    assert!(rows.windows(2).all(|w| w[0]["latency_ms"].as_f64().unwrap() >= w[1]["latency_ms"].as_f64().unwrap()) && rows.iter().all(|r| r["name"].is_string()), "ordered by latency, every column");
    assert_eq!(found["places"]["m-ams"].as_i64().unwrap(), behind.len() as i64, "counted where they are, for the map");
    let found = ask(format!("everything behind {ag} that is not ok")).await;
    bounded(&found, "a large walk with a condition");
    assert!(found["total"].as_i64().unwrap() >= 3, "the broken ones behind {ag}: {}", found["total"]);

    // Diagnose and the path between two devices walk the backbone in the app.
    let path = api.get("/v1/topology/path?from=ag1-ams&to=ag1-fra").await;
    bounded(&path, "a path");
    assert!(path["engine"] == "app" && len(&path["path"]) >= 2, "a path: {path}");
    let wh = api.get("/v1/where").await;
    bounded(&wh, "Diagnose");

    // And the coordinator was sent no graph statement at all.
    let stats = api.http.get(format!("{}/v1/queries/stats?limit=5000", cluster.url)).send().await.unwrap().json::<Value>().await.unwrap();
    let sent = stats["statements"].as_array().unwrap_or_else(|| panic!("the coordinator's statements: {stats}"));
    assert!(sent.len() > 10, "the coordinator counts what it is sent");
    for s in sent {
        let t = s["sql"].as_str().unwrap_or("");
        assert!(!t.contains("REACHABLE(") && !t.contains("ppr(") && !t.contains("PATH("), "a graph statement went to the coordinator: {}", &t[..t.len().min(300)]);
    }
    app.stop();
}
