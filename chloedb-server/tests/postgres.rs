//! Strata following Postgres by logical replication
//! (`chloedb-server follow-postgres`), against a real Postgres: a server
//! started for the test from the binaries installed (`initdb` and
//! `postgres` on the PATH, or in /usr/lib/postgresql/*/bin and Homebrew's
//! places), or the one `CHLOEDB_TEST_POSTGRES_URL` names (a superuser's
//! URL, of a server with `wal_level = logical`). Without either the tests
//! say so and pass, but on GitHub's Linux runners, which have Postgres.
//!
//! The follower copies a publication's tables, then applies inserts,
//! updates (one leaving a large TOASTed value out), key changes,
//! deletes, a TRUNCATE, an added and a renamed column, a transaction of
//! thousands of rows, and a table that joins the publication, into one
//! server and into a cluster of two shards; stopped in the middle of a
//! transaction it writes in steps, and started again, it applies each
//! change once.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use chloedb::db::Db;
use chloedb::storage::DbOptions;
use chloedb_server::auth::Access;
use chloedb_server::coord::Coordinator;
use chloedb_server::postgres::wire::{Config, Conn};
use chloedb_server::postgres::{self, Options};
use chloedb_server::service::{AgentConfig, Backend, Service};
use chloedb_server::{server, slots};
use tokio::net::TcpListener;

/// A Postgres for a test: its URL as the superuser, and the server
/// process if the test started it.
struct Pg {
    url: String,
    child: Option<Child>,
    _dir: Option<tempfile::TempDir>,
}

/// How long a test's Postgres has to stop before it is killed.
const PG_STOP: Duration = Duration::from_secs(10);

impl Drop for Pg {
    /// Stops the server by its fast shutdown (SIGINT, which `runuser`
    /// passes on), which removes its System V shared memory segment;
    /// killed (SIGKILL), it leaves the segment behind, and a few runs of
    /// the tests use up the system's (32 on macOS, `kern.sysv.shmmni`),
    /// after which `initdb` fails. Killed only if it has not stopped
    /// within [`PG_STOP`].
    fn drop(&mut self) {
        let Some(c) = &mut self.child else { return };
        if let Ok(pid) = libc::pid_t::try_from(c.id()) {
            unsafe { libc::kill(pid, libc::SIGINT) };
            let until = Instant::now() + PG_STOP;
            while Instant::now() < until {
                if let Ok(Some(_)) = c.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn bin_dir() -> Option<PathBuf> {
    let has = |d: &Path| d.join("initdb").exists() && d.join("postgres").exists();
    if let Ok(path) = std::env::var("PATH") {
        if let Some(d) = std::env::split_paths(&path).find(|d| has(d)) {
            return Some(d);
        }
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for root in ["/usr/lib/postgresql", "/opt/homebrew/opt", "/usr/local/opt"] {
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                let d = if root == "/usr/lib/postgresql" { e.path().join("bin") } else if n.starts_with("postgresql") { e.path().join("bin") } else { continue };
                if has(&d) {
                    found.push(d);
                }
            }
        }
    }
    found.sort();
    found.pop()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Runs `cmd` as the user that owns Postgres's files: this one, or for
/// root (which Postgres refuses) user `postgres`.
fn as_owner(cmd: &Path) -> Command {
    if unsafe { libc::geteuid() } == 0 {
        let mut c = Command::new("runuser");
        c.args(["-u", "postgres", "--"]).arg(cmd);
        c
    } else {
        Command::new(cmd)
    }
}

const PASSWORD: &str = "s3cret pass";

/// A Postgres to test with, or `None` (said) when there is none here.
async fn postgres() -> Option<Pg> {
    if let Ok(url) = chloedb::env::var("CHLOEDB_TEST_POSTGRES_URL") {
        return Some(Pg { url, child: None, _dir: None });
    }
    let Some(bin) = bin_dir() else {
        // GitHub's own Linux runners have it, and so does CI's self-hosted
        // runner (.github/ci-runner/provision.sh); CI sets
        // CHLOEDB_TEST_POSTGRES_REQUIRED, so that these tests fail there
        // rather than pass without running (audit 4, X-I3).
        let required = std::env::var("CHLOEDB_TEST_POSTGRES_REQUIRED").is_ok_and(|v| !v.is_empty() && v != "0") || (std::env::var("RUNNER_ENVIRONMENT").is_ok_and(|e| e == "github-hosted") && cfg!(target_os = "linux"));
        assert!(!required, "no Postgres here (initdb not found, CHLOEDB_TEST_POSTGRES_URL not set), and CHLOEDB_TEST_POSTGRES_REQUIRED or a GitHub-hosted Linux runner requires one");
        eprintln!("no Postgres here (initdb not found, CHLOEDB_TEST_POSTGRES_URL not set): skipping");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = unsafe { libc::geteuid() } == 0;
    let data = dir.path().join("data");
    let pw = dir.path().join("pw");
    std::fs::write(&pw, PASSWORD).unwrap();
    if root {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&pw, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let out = as_owner(&bin.join("initdb"))
        .arg("-D")
        .arg(&data)
        .args(["-U", "postgres", "--auth-local=trust", "--auth-host=scram-sha-256", "-E", "UTF8", "--no-locale"])
        .arg(format!("--pwfile={}", pw.display()))
        .output()
        .unwrap();
    assert!(out.status.success(), "initdb: {}", String::from_utf8_lossy(&out.stderr));
    // A role signing in with MD5 first, then everyone by SCRAM.
    let hba = data.join("pg_hba.conf");
    let was = std::fs::read_to_string(&hba).unwrap();
    std::fs::write(&hba, format!("host all md5user 127.0.0.1/32 md5\nhost replication md5user 127.0.0.1/32 md5\n{was}\nhost replication all 127.0.0.1/32 scram-sha-256\n")).unwrap();
    // TLS with a certificate of a CA made here, when openssl is.
    let tls = tls_files(&data);
    let port = free_port();
    let mut cmd = as_owner(&bin.join("postgres"));
    cmd.arg("-D").arg(&data).args(["-p", &port.to_string(), "-c", "listen_addresses=127.0.0.1", "-c", "wal_level=logical", "-c", "max_wal_senders=20", "-c", "max_replication_slots=20", "-c", "fsync=off"]);
    cmd.arg("-c").arg(format!("unix_socket_directories={}", dir.path().display()));
    if tls {
        cmd.args(["-c", "ssl=on", "-c", "ssl_cert_file=server.crt", "-c", "ssl_key_file=server.key"]);
    }
    let child = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let url = format!("postgres://postgres:{}@127.0.0.1:{port}/postgres?sslmode=disable", PASSWORD.replace(' ', "%20"));
    let pg = Pg { url, child: Some(child), _dir: Some(dir) };
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        match Conn::connect(&Config::parse(&pg.url).unwrap(), false).await {
            Ok(c) => {
                c.close().await;
                break;
            }
            Err(e) => {
                assert!(Instant::now() < until, "Postgres did not start: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Some(pg)
}

/// Makes a CA and a server certificate for localhost in `data`; false
/// without openssl.
fn tls_files(data: &Path) -> bool {
    let run = |args: &[&str]| Command::new("openssl").args(args).current_dir(data).output().is_ok_and(|o| o.status.success());
    let ok = run(&["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=test-ca", "-keyout", "ca.key", "-out", "ca.crt"])
        && run(&["req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost", "-keyout", "server.key", "-out", "server.csr"])
        && std::fs::write(data.join("ext.cnf"), "subjectAltName=DNS:localhost,IP:127.0.0.1\n").is_ok()
        && run(&["x509", "-req", "-in", "server.csr", "-CA", "ca.crt", "-CAkey", "ca.key", "-CAcreateserial", "-days", "2", "-extfile", "ext.cnf", "-out", "server.crt"]);
    if ok && unsafe { libc::geteuid() } == 0 {
        let _ = Command::new("chown").args(["-R", "postgres"]).arg(data).status();
    }
    if ok {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(data.join("server.key"), std::fs::Permissions::from_mode(0o600));
    }
    ok
}

/// A database of this name, made afresh, as a URL.
async fn database(pg: &Pg, name: &str) -> String {
    let mut c = Conn::connect(&Config::parse(&pg.url).unwrap(), false).await.unwrap();
    c.query(&format!("DROP DATABASE IF EXISTS {name}")).await.unwrap();
    c.query(&format!("CREATE DATABASE {name}")).await.unwrap();
    c.close().await;
    let (base, query) = pg.url.split_once('?').unwrap_or((&pg.url, ""));
    let base = base.rsplit_once('/').unwrap().0;
    format!("{base}/{name}?{query}")
}

async fn exec(url: &str, sql: &str) -> chloedb_server::postgres::wire::Rows {
    let mut c = Conn::connect(&Config::parse(url).unwrap(), false).await.unwrap();
    let r = c.query(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    c.close().await;
    r
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: chloedb_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn empty(dir: &Path, name: &str) -> Service {
    let db = Db::create_empty(&dir.join(name), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(20))
}

async fn http(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc, l, Access::default()));
    url
}

/// One server, or a coordinator over two shards: its HTTP URL.
async fn strata(dir: &Path, shards: usize) -> String {
    if shards == 0 {
        return http(Arc::new(empty(dir, "one.strata"))).await;
    }
    let mut urls = Vec::new();
    for i in 0..shards {
        let name = format!("s{i}.strata");
        let db = Db::create_empty(&dir.join(&name), DbOptions::default()).unwrap();
        db.storage().start_seq_no().unwrap();
        let schema = (*db.schema()).clone();
        let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(20));
        svc.keep_slots_in(slots::file_of(&dir.join(&name))).unwrap();
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://{}", l.local_addr().unwrap()));
        tokio::spawn(server::serve_grpc(Arc::new(svc), l, Access::default()));
    }
    let c = Coordinator::connect(urls).await.unwrap();
    let schema = c.schema.clone();
    http(Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(20)))).await
}

async fn call(url: &str, method: &str, path: &str, body: Option<Json>) -> (u16, Json) {
    let c = chloedb_server::http_client().build().unwrap();
    let r = match method {
        "GET" => c.get(format!("{url}{path}")),
        _ => c.post(format!("{url}{path}")).json(&body.unwrap_or(Json::Null)),
    };
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Json::Null))
}

/// Strata's rows of `sql`, as JSON arrays; empty while the table is missing.
async fn strata_rows(url: &str, sql: &str) -> Vec<Vec<Json>> {
    let (s, j) = call(url, "POST", "/v1/sql", Some(json!({"sql": sql}))).await;
    if s != 200 {
        return Vec::new();
    }
    j["rows"].as_array().cloned().unwrap_or_default().into_iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect()
}

fn options(source: &str, target: &str, name: &str) -> Options {
    Options {
        source: source.to_string(),
        source_ca: None,
        publication: "strata".into(),
        slot: None,
        target: target.to_string(),
        target_token: None,
        target_ca: None,
        name: name.to_string(),
        batch: 500,
        copy: false,
        copy_workers: 2,
        crash_after: None,
    }
}

/// Postgres's rows of table `t` as Strata should hold them: `cols`, by key.
async fn pg_rows(src: &str, sql: &str) -> Vec<Vec<Option<String>>> {
    exec(src, sql).await.rows
}

/// Waits until Strata's rows of `strata_sql` equal Postgres's of `pg_sql`
/// (each value compared as text).
async fn converges(src: &str, dst: &str, pg_sql: &str, strata_sql: &str) {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        // Compared as sorted text: `ORDER BY id` in Postgres orders by the
        // text column `id::text` names, Strata by the integer.
        let mut want = pg_rows(src, pg_sql).await;
        want.sort();
        let mut have: Vec<Vec<Option<String>>> = strata_rows(dst, strata_sql)
            .await
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| match v {
                        Json::Null => None,
                        Json::String(s) => Some(s),
                        Json::Number(n) => Some(n.as_f64().map_or(n.to_string(), |f| if f.fract() == 0.0 { format!("{}", f as i64) } else { f.to_string() })),
                        v => Some(v.to_string()),
                    })
                    .collect()
            })
            .collect();
        have.sort();
        if want == have {
            return;
        }
        if Instant::now() > until {
            let show = |v: &Vec<Vec<Option<String>>>| v.iter().take(8).map(|r| format!("{r:?}")).collect::<Vec<_>>().join("\n");
            panic!("ChloeDB did not converge on {pg_sql}: {} rows wanted, {} held\nwanted:\n{}\nheld:\n{}", want.len(), have.len(), show(&want), show(&have));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// What a test follower runs as: a task, stopped when dropped.
struct Running(tokio::task::JoinHandle<Result<(), chloedb_server::ApiError>>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start(o: Options) -> Running {
    Running(tokio::spawn(postgres::run(o)))
}

/// Text no compression shrinks, so that Postgres keeps it out of line (TOAST).
fn incompressible(n: usize, seed: u64) -> String {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (b'a' + (x % 26) as u8) as char
        })
        .collect()
}

const ITEMS_PG: &str = "SELECT id::text, title, price::float8::text, n::text FROM items ORDER BY id";
const ITEMS: &str = "SELECT id, title, price, n FROM items ORDER BY id LIMIT 100000";

async fn follows_changes_into(shards: usize) {
    let Some(pg) = postgres().await else { return };
    let src = database(&pg, &format!("follow{shards}")).await;
    exec(
        &src,
        "CREATE TABLE items (id int PRIMARY KEY, title text, body text, price numeric(10,2), n bigint, tags text[], at timestamptz, meta jsonb, ok boolean, code uuid);
         CREATE TABLE pairs (a int, b text, v text, PRIMARY KEY (a, b));
         CREATE TABLE skipped (id int PRIMARY KEY, x text);
         INSERT INTO items SELECT i, 'item number ' || i, 'body ' || i, i * 1.5, i * 10, ARRAY['t' || (i % 3), 'all'], '2024-05-06 07:08:09.123456+00', jsonb_build_object('k', i), i % 2 = 0, md5(i::text)::uuid FROM generate_series(1, 300) i;
         INSERT INTO pairs SELECT i % 10, 'b' || i, 'v' || i FROM generate_series(1, 50) i;
         INSERT INTO skipped VALUES (1, 'not published');
         CREATE PUBLICATION strata FOR TABLE items, pairs;",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let dst = strata(dir.path(), shards).await;
    let run = start(options(&src, &dst, "shop"));

    // The copy, with the types mapped.
    converges(&src, &dst, ITEMS_PG, ITEMS).await;
    converges(&src, &dst, "SELECT a::text, b, v FROM pairs ORDER BY a, b", "SELECT a, b, v FROM pairs ORDER BY a, b LIMIT 1000").await;
    let (_, tables) = call(&dst, "GET", "/v1/tables", None).await;
    let names: Vec<&str> = tables["tables"].as_array().unwrap().iter().filter_map(|t| t["table"].as_str()).collect();
    assert!(names.contains(&"items") && names.contains(&"pairs") && !names.contains(&"skipped"), "{names:?}");
    let row = strata_rows(&dst, "SELECT tags, at, meta, ok, code FROM items WHERE id = 4").await;
    assert_eq!(row[0][0], json!(["t1", "all"]));
    assert_eq!(row[0][1], json!("2024-05-06T07:08:09.123456Z"));
    assert_eq!(row[0][2], json!(r#"{"k":4}"#), "SQL gives JSON columns as their text");
    assert_eq!(row[0][3], json!(true));
    let found = strata_rows(&dst, "SELECT id FROM items WHERE MATCH(title, 'number') ORDER BY id LIMIT 3").await;
    assert_eq!(found.len(), 3, "titles are searchable text");
    let pairs = strata_rows(&dst, "SELECT _key FROM pairs WHERE a = 1 AND b = 'b1'").await;
    assert_eq!(pairs, vec![vec![json!("1:b1")]]);

    // Changes: inserts, updates, a key changed, deletes.
    exec(
        &src,
        "INSERT INTO items (id, title, price, n) VALUES (1001, 'a new one', 9.99, 1);
         UPDATE items SET title = 'renamed item', price = 2.5 WHERE id = 7;
         UPDATE items SET id = 2001 WHERE id = 8;
         DELETE FROM items WHERE id BETWEEN 10 AND 19;
         UPDATE pairs SET v = 'changed' WHERE a = 2;
         DELETE FROM pairs WHERE a = 3;",
    )
    .await;
    converges(&src, &dst, ITEMS_PG, ITEMS).await;
    converges(&src, &dst, "SELECT a::text, b, v FROM pairs ORDER BY a, b", "SELECT a, b, v FROM pairs ORDER BY a, b LIMIT 1000").await;

    // A large value an update leaves out (unchanged TOAST): kept.
    let big = incompressible(40_000, 1);
    exec(&src, &format!("UPDATE items SET body = '{big}' WHERE id = 20")).await;
    exec(&src, "UPDATE items SET price = 77 WHERE id = 20").await;
    converges(&src, &dst, ITEMS_PG, ITEMS).await;
    let body = strata_rows(&dst, "SELECT body FROM items WHERE id = 20").await;
    assert_eq!(body[0][0].as_str().map(str::len), Some(40_000), "the unchanged value is kept");

    // Schema changes: a column added, one renamed.
    exec(&src, "ALTER TABLE items ADD COLUMN rating int; UPDATE items SET rating = id % 5 WHERE id <= 30").await;
    converges(&src, &dst, "SELECT id::text, rating::text FROM items ORDER BY id", "SELECT id, rating FROM items ORDER BY id LIMIT 100000").await;
    exec(&src, "ALTER TABLE items RENAME COLUMN rating TO stars; UPDATE items SET stars = 9 WHERE id = 1").await;
    converges(&src, &dst, "SELECT id::text, stars::text FROM items ORDER BY id", "SELECT id, stars FROM items ORDER BY id LIMIT 100000").await;

    // A transaction of thousands of rows, written in steps; a TRUNCATE.
    exec(&src, "INSERT INTO items (id, title, n) SELECT i, 'bulk ' || i, i FROM generate_series(5000, 7499) i").await;
    converges(&src, &dst, ITEMS_PG, ITEMS).await;
    exec(&src, "BEGIN; TRUNCATE pairs; INSERT INTO pairs SELECT 1, 'x' || i, 'after' FROM generate_series(1, 5) i; COMMIT").await;
    converges(&src, &dst, "SELECT a::text, b, v FROM pairs ORDER BY a, b", "SELECT a, b, v FROM pairs ORDER BY a, b LIMIT 1000").await;

    // A table that joins the publication, with rows from before.
    exec(&src, "CREATE TABLE notes (id bigint PRIMARY KEY, text text); INSERT INTO notes SELECT i, 'note ' || i FROM generate_series(1, 40) i; ALTER PUBLICATION strata ADD TABLE notes; INSERT INTO notes VALUES (41, 'after joining')").await;
    converges(&src, &dst, "SELECT id::text, text FROM notes ORDER BY id", "SELECT id, text FROM notes ORDER BY id LIMIT 1000").await;

    // Stopped and started again: it goes on from where it was.
    drop(run);
    exec(&src, "UPDATE items SET title = 'while stopped' WHERE id < 50; DELETE FROM notes WHERE id > 30").await;
    let run = start(options(&src, &dst, "shop"));
    converges(&src, &dst, ITEMS_PG, ITEMS).await;
    converges(&src, &dst, "SELECT id::text, text FROM notes ORDER BY id", "SELECT id, text FROM notes ORDER BY id LIMIT 1000").await;

    // Stopped in the middle of a transaction written in steps, after its
    // TRUNCATE and its first step: started again, the TRUNCATE is not
    // done again (it would delete the step's rows, which are not written
    // again).
    drop(run);
    let crash = Arc::new(AtomicU64::new(2));
    let mut o = options(&src, &dst, "shop");
    o.crash_after = Some(crash.clone());
    let run = start(o);
    exec(&src, "BEGIN; TRUNCATE pairs; INSERT INTO pairs SELECT i, 'step', 'v' FROM generate_series(1, 2500) i; COMMIT").await;
    let until = Instant::now() + Duration::from_secs(60);
    while !run.0.is_finished() {
        assert!(Instant::now() < until, "the follower did not stop as asked");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let held = strata_rows(&dst, "SELECT count(*) FROM pairs").await;
    assert_eq!(held[0][0], json!(1000), "stopped after the TRUNCATE and the first step of 1,000 rows");
    let _run = start(options(&src, &dst, "shop"));
    converges(&src, &dst, "SELECT a::text, b, v FROM pairs ORDER BY a, b", "SELECT a, b, v FROM pairs ORDER BY a, b LIMIT 10000").await;

    // What it applied is in _sources.
    let (_, s) = call(&dst, "GET", "/v1/sources", None).await;
    let names: Vec<&str> = s["sources"].as_array().unwrap().iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"postgres/shop/copy"), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with("/copied")), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with("/notes")), "{names:?}");
    drop(_run);
    let dropped = postgres::drop_slot(options(&src, &dst, "shop")).await.unwrap();
    assert_eq!(dropped["dropped"], json!(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_follows_postgres() {
    follows_changes_into(0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cluster_follows_postgres() {
    follows_changes_into(2).await;
}

/// SCRAM, MD5 and TLS sign-ins, and the refusals that say what to fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signs_in_and_says_what_is_missing() {
    let Some(pg) = postgres().await else { return };
    let src = database(&pg, "signin").await;
    // SCRAM (the superuser's, by the URL).
    let rows = exec(&src, "SELECT current_user").await;
    assert_eq!(rows.rows[0][0].as_deref(), Some("postgres"));
    // MD5.
    exec(&src, "SET password_encryption = 'md5'; CREATE ROLE md5user LOGIN REPLICATION PASSWORD 'md5 pw'").await;
    let md5 = src.replacen(&format!("postgres:{}", PASSWORD.replace(' ', "%20")), "md5user:md5%20pw", 1);
    assert_eq!(exec(&md5, "SELECT current_user").await.rows[0][0].as_deref(), Some("md5user"));
    // A wrong password.
    let wrong = src.replacen(&PASSWORD.replace(' ', "%20"), "nope", 1);
    let e = Conn::connect(&Config::parse(&wrong).unwrap(), false).await.err().unwrap();
    assert!(e.message.contains("password authentication failed"), "{e}");
    // TLS, when the server has it.
    if exec(&src, "SHOW ssl").await.rows[0][0].as_deref() == Some("on") {
        let data_dir = exec(&src, "SHOW data_directory").await.rows[0][0].clone().unwrap();
        let ca = std::fs::read(Path::new(&data_dir).join("ca.crt")).unwrap();
        for mode in ["require", "verify-ca", "verify-full"] {
            let mut cfg = Config::parse(&src.replace("sslmode=disable", &format!("sslmode={mode}"))).unwrap();
            cfg.root_cert = Some(ca.clone());
            if mode == "verify-full" {
                cfg.host = "localhost".into();
            }
            let mut c = Conn::connect(&cfg, false).await.unwrap_or_else(|e| panic!("sslmode={mode}: {e}"));
            assert_eq!(c.query("SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()").await.unwrap().rows[0][0].as_deref(), Some("true"));
            c.close().await;
        }
        // Checked against public CAs, a certificate of this CA is refused.
        let cfg = Config::parse(&src.replace("sslmode=disable", "sslmode=verify-full").replace("127.0.0.1", "localhost")).unwrap();
        assert!(Conn::connect(&cfg, false).await.is_err());
    }
    // A database without the publication says how to make it.
    let dir = tempfile::tempdir().unwrap();
    let dst = strata(dir.path(), 0).await;
    let e = postgres::run(options(&src, &dst, "x")).await.unwrap_err();
    assert!(e.message.contains("CREATE PUBLICATION"), "{e}");
    // A table without a key says how to give it one.
    exec(&src, "CREATE TABLE nokey (a int, b text); CREATE PUBLICATION strata FOR TABLE nokey").await;
    let e = postgres::run(options(&src, &dst, "x")).await.unwrap_err();
    assert!(e.message.contains("no primary key"), "{e}");
    let _ = postgres::drop_slot(options(&src, &dst, "x")).await;
}
