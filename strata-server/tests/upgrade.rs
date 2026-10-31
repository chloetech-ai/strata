//! `strata-server upgrade PATH` copies a database written before field ids,
//! which the server refuses to serve, into this version's format in its
//! place, and a second run finds nothing to do.

use std::path::Path;
use std::process::Command;

use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;

/// Two tables (`t` and `u`) written by Strata at 835ca9a
/// (`strata/tests/fixtures/README.md`), copied into `dir`.
fn old_database(dir: &Path) -> std::path::PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../strata/tests/fixtures/format2");
    for name in ["db.strata", "db.strata-wal"] {
        std::fs::copy(fixture.join(name), dir.join(name)).unwrap();
    }
    dir.join("db.strata")
}

fn server(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_strata-server")).args(args).output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn upgrade_copies_a_database_from_before_field_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = old_database(dir.path());
    let p = path.to_str().unwrap();
    let (ok, out) = server(&["upgrade", p]);
    assert!(ok, "{out}");
    assert!(out.contains("copied 44 rows of 2 tables"), "{out}");
    assert!(dir.path().join("db.strata.before-field-ids").exists());
    let db = Db::open(&path, DbOptions::default()).unwrap();
    assert_eq!(db.query("SELECT COUNT(*) FROM t", &Params::new()).unwrap().rows, [vec![strata::Value::Int(34)]]);
    drop(db);
    let (ok, out) = server(&["upgrade", p]);
    assert!(ok && out.contains("needs no upgrade"), "{out}");
}
