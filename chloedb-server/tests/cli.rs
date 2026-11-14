//! The `chloedb-server` program as a newcomer and a script run it: what
//! `--help` lists, `--schema` given inline, and `admin` piped into a reader
//! that stops early.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_chloedb-server");
const SCHEMA: &str = r#"{"table": "notes", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "body", "type": "text"}]}"#;

fn bin() -> Command {
    let mut c = Command::new(BIN);
    for v in ["CHLOEDB_URL", "CHLOEDB_KEY", "CHLOEDB_TOKEN", "STRATA_URL", "STRATA_KEY", "STRATA_TOKEN"] {
        c.env_remove(v);
    }
    c
}

/// A server on a free port, killed when dropped.
struct Server(std::process::Child, String);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(db: &std::path::Path, extra: &[&str]) -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");
    let child = bin()
        .arg("--db")
        .arg(db)
        .args(["--listen", &format!("127.0.0.1:{port}"), "--grpc", "off", "--log", "warn", "--start-apps", "off", "--no-fetch-models"])
        .args(extra)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let server = Server(child, url);
    let t0 = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    server
}

fn tables(url: &str) -> serde_json::Value {
    let out = bin().args(["admin", "--url", url, "--json", "call", "GET", "/v1/tables"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `--schema` takes the JSON itself, as its help says, besides a file.
#[test]
fn schema_given_as_json_creates_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let rows = dir.path().join("rows.jsonl");
    std::fs::write(&rows, "{\"id\": \"n1\", \"body\": \"first note\"}\n{\"id\": \"n2\", \"body\": \"second note\"}\n").unwrap();
    let db = dir.path().join("imported.chloedb");
    let out = bin().arg("import").arg("--db").arg(&db).args(["--schema", SCHEMA]).arg(&rows).output().unwrap();
    assert!(out.status.success(), "import --schema JSON: {}", String::from_utf8_lossy(&out.stderr));

    let served = dir.path().join("served.chloedb");
    let s = serve(&served, &["--schema", SCHEMA]);
    let t = tables(&s.1);
    assert!(t.to_string().contains("\"notes\""), "{t}");

    // A file still works, and a path that is not there says which.
    let file = dir.path().join("schema.json");
    std::fs::write(&file, SCHEMA).unwrap();
    let out = bin().arg("import").arg("--db").arg(dir.path().join("from-file.chloedb")).arg("--schema").arg(&file).arg(&rows).output().unwrap();
    assert!(out.status.success(), "import --schema FILE: {}", String::from_utf8_lossy(&out.stderr));
    let out = bin().arg("import").arg("--db").arg(dir.path().join("none.chloedb")).args(["--schema", "missing.json"]).arg(&rows).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--schema missing.json"), "{}", String::from_utf8_lossy(&out.stderr));
}

/// `chloedb-server admin ... | head -1`: the reader closes the pipe before
/// the answer is written, and the command ends quietly, where `println!`
/// panicked with exit code 101.
#[test]
fn admin_into_a_closed_pipe_exits_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let s = serve(&dir.path().join("pipe.chloedb"), &["--demo", "200"]);
    for args in [&["catalog"][..], &["--json", "catalog"][..]] {
        let mut child = bin().args(["admin", "--url", &s.1]).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        drop(child.stdout.take()); // the reader is gone before the answer comes
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "admin {args:?}: {err}");
        assert_eq!(out.status.code(), Some(0), "admin {args:?}: {err}");
    }
    // A route that does not exist answers the API's error shape, and admin
    // prints it and exits 1. (An answer that is not JSON, such as an empty
    // body, says its status instead: `admin_cli`'s unit tests.)
    let out = bin().args(["admin", "--url", &s.1, "--json", "call", "GET", "/v1/nope"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let j: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(j["error_code"], "not_found", "{j}");
    assert!(j["error"].as_str().is_some_and(|e| e.contains("/v1/nope")), "{j}");
}

/// `-h` lists the options of a first run; `--help` every option, by topic.
#[test]
fn help_is_short_and_grouped() {
    let short = bin().arg("-h").output().unwrap();
    assert!(short.status.success());
    let short = String::from_utf8_lossy(&short.stdout).into_owned();
    let long = bin().arg("--help").output().unwrap();
    let long = String::from_utf8_lossy(&long.stdout).into_owned();
    for flag in ["--db", "--schema", "--demo", "--listen", "--token"] {
        assert!(short.contains(flag), "-h has no {flag}:\n{short}");
    }
    assert!(!short.contains("--hedge-ms"), "-h lists a cluster's option:\n{short}");
    assert!(short.lines().count() < 80, "-h is {} lines:\n{short}", short.lines().count());
    for heading in ["Start:", "Replication:", "Security:", "Limits and timeouts:"] {
        assert!(long.contains(heading), "--help has no {heading}");
    }
    assert!(long.contains("--hedge-ms") && long.contains("--db"));
}

/// What a start that fails says, and that it leaves nothing behind
/// (ONB-13): an address in use or no address at all is found before the
/// database is made, a folder that holds no database and a file that is
/// not one are refused without a readers' folder beside them, and a
/// missing folder is named.
#[test]
fn a_start_that_fails_says_why_and_leaves_no_files() {
    let dir = tempfile::tempdir().unwrap();
    let start = |db: &std::path::Path, listen: &str| {
        let out = bin()
            .arg("--db")
            .arg(db)
            .args(["--listen", listen, "--grpc", "off", "--log", "warn", "--start-apps", "off", "--no-fetch-models"])
            .stdout(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{} started", db.display());
        String::from_utf8_lossy(&out.stderr).to_string()
    };
    let files = |d: &std::path::Path| {
        let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    };
    let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();

    // A port in use: the hint, and no database.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let e = start(&dir.path().join("a.chloedb"), &addr);
    assert!(e.contains("in use") && e.contains("--listen") && e.contains("lsof"), "{e}");
    assert_eq!(files(dir.path()), Vec::<String>::new());
    // A port that is no port.
    let e = start(&dir.path().join("a.chloedb"), "127.0.0.1:99999");
    assert!(e.contains("127.0.0.1:99999") && e.contains("--listen"), "{e}");
    assert_eq!(files(dir.path()), Vec::<String>::new());

    // A folder that holds no database.
    let empty = dir.path().join("adir");
    std::fs::create_dir(&empty).unwrap();
    let e = start(&empty, &free());
    assert!(e.contains("adir") && e.contains("holds no ChloeDB database"), "{e}");
    assert_eq!(files(&empty), Vec::<String>::new());

    // A file that is not a database: refused, and no readers' folder.
    let text = dir.path().join("notdb.txt");
    std::fs::write(&text, "not a database\n").unwrap();
    let e = start(&text, &free());
    assert!(e.contains("notdb.txt"), "{e}");
    assert_eq!(files(dir.path()), ["adir", "notdb.txt"]);

    // A folder that is not there: named, with what makes it.
    let e = start(&dir.path().join("no/such/x.chloedb"), &free());
    assert!(e.contains("no/such") && e.contains("mkdir -p"), "{e}");
    assert_eq!(files(dir.path()), ["adir", "notdb.txt"]);
}
