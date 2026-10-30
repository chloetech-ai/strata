//! Rank profiles (`CREATE RANK PROFILE ... ON docs AS ...`, then `ORDER BY
//! rank_profile('name', ...)`): a query by a profile answers as the query
//! written out in full; profiles are kept with their table, replaced and
//! dropped, logged and replayed after a crash, shipped to a replica and
//! carried by copies; and a plan names its profile, which a database whose
//! profile differs refuses.

use std::path::Path;

use strata::data::{self, col};
use strata::db::{Db, SqlResult};
use strata::query::exec::ResultSet;
use strata::query::ir::{Plan, FEATURES, RANK_PHASES, RANK_PROFILES};
use strata::query::Params;
use strata::storage::{Applied, CheckpointOptions, DbOptions, Layout, LogRead, RankProfile};
use strata::value::Value;
use strata::VectorIndexKind;

const N: usize = 800;
const DIM: usize = 16;

fn options() -> DbOptions {
    DbOptions { segment_max_docs: 200, memtable_max_docs: 1_000_000, ..DbOptions::default() }
}

fn create(path: &Path, opts: DbOptions) -> (Db, Params) {
    let db = Db::create(path, data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    let mut docs = data::synthetic(N, DIM, 5);
    for d in docs.iter_mut().step_by(7) {
        d[col::RATING] = Value::Null;
    }
    db.load(docs.iter().cloned()).unwrap();
    let words = |i: usize, n: usize| docs[i][col::TITLE].as_str().unwrap().split(' ').take(n).collect::<Vec<_>>().join(" ");
    let params = Params::new().set("q", docs[42][col::EMB].clone()).set("t", Value::Str(words(42, 3))).set("u", Value::Str(words(7, 2)));
    (db, params)
}

fn sql(db: &Db, text: &str) -> SqlResult {
    db.sql(text, &Params::new()).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn refused(db: &Db, text: &str, params: &Params, says: &str) {
    let e = db.sql(text, params).err().unwrap_or_else(|| panic!("{text}: not refused")).to_string();
    assert!(e.contains(says), "{text}: {e}");
}

fn query(db: &Db, text: &str, params: &Params) -> ResultSet {
    db.query(text, params).unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// The same columns, rows and scores.
fn same(what: &str, a: &ResultSet, b: &ResultSet) {
    assert_eq!(a.columns, b.columns, "{what}: columns");
    assert_eq!(a.rows, b.rows, "{what}: rows");
    assert_eq!(a.scores.len(), b.scores.len(), "{what}: scores");
    for (i, (x, y)) in a.scores.iter().zip(&b.scores).enumerate() {
        assert!(x == y || (x.is_nan() && y.is_nan()), "{what}: row {i}: {x} vs {y}");
    }
    assert!(!a.rows.is_empty(), "{what}: no rows");
}

const PROFILES: [&str; 4] = [
    "CREATE RANK PROFILE boost ON docs AS SELECT * FROM docs ORDER BY bm25(body, :t) * (1 + :w * coalesce(rating, 0)) DESC WITH (w => 0.2)",
    "CREATE RANK PROFILE phased ON docs AS
       WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 100)
       SELECT * FROM top ORDER BY similarity(emb, :q) + :w * coalesce(rating, 0) DESC
       WITH (w = 0.5)",
    "CREATE RANK PROFILE three ON docs AS
       WITH a AS (SELECT * FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 200),
            b AS (SELECT * FROM a ORDER BY coalesce(rating, 0) DESC LIMIT 50)
       SELECT * FROM b ORDER BY minmax(bm25(body, :t)) + minmax(similarity(emb, :q)) DESC",
    "CREATE RANK PROFILE oldest ON docs AS SELECT * FROM docs ORDER BY year",
];

#[test]
fn a_query_by_a_profile_answers_as_written_out() {
    let dir = tempfile::tempdir().unwrap();
    let (db, params) = create(&dir.path().join("db.strata"), options());
    for p in PROFILES {
        sql(&db, p);
    }
    let names: Vec<String> = db.storage().rank_profiles("DOCS").unwrap().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["boost", "oldest", "phased", "three"]);
    let phased = |w: &str, first: &str, cut: &str| format!(
        "WITH top AS (SELECT * FROM docs {first} ORDER BY bm25(body, :t) DESC LIMIT 100) SELECT {{cols}} FROM top ORDER BY similarity(emb, :q) + {w} * coalesce(rating, 0) DESC {cut}"
    );
    let cases: Vec<(String, String)> = vec![
        // Its default, and the query's :t.
        ("SELECT id, score() FROM docs ORDER BY rank_profile('boost') LIMIT 10".into(), "SELECT id, score() FROM docs ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 10".into()),
        // Arguments: a value, a negative one, a parameter of the query.
        (
            "SELECT id, rating, score() FROM docs WHERE year >= 2005 ORDER BY rank_profile('BOOST', w => 0.7, t => :u) LIMIT 8 OFFSET 2".into(),
            "SELECT id, rating, score() FROM docs WHERE year >= 2005 ORDER BY bm25(body, :u) * (1 + 0.7 * coalesce(rating, 0)) DESC LIMIT 8 OFFSET 2".into(),
        ),
        ("SELECT id, score() FROM docs ORDER BY rank_profile('boost', w => -0.05) DESC LIMIT 10".into(), "SELECT id, score() FROM docs ORDER BY bm25(body, :t) * (1 + -0.05 * coalesce(rating, 0)) DESC LIMIT 10".into()),
        // Phases, under the query's WHERE, cut by its LIMIT and OFFSET.
        ("SELECT id, score() FROM docs ORDER BY rank_profile('phased') LIMIT 10".into(), phased("0.5", "", "LIMIT 10").replace("{cols}", "id, score()")),
        (
            "SELECT id, year, score() FROM docs WHERE year >= 2005 ORDER BY rank_profile('phased', w => 0) LIMIT 7 OFFSET 3".into(),
            phased("0", "WHERE year >= 2005", "LIMIT 7 OFFSET 3").replace("{cols}", "id, year, score()"),
        ),
        // features() lists the profile's inputs.
        ("SELECT id, features() FROM docs ORDER BY rank_profile('phased', w => 1.5) LIMIT 5".into(), phased("1.5", "", "LIMIT 5").replace("{cols}", "id, features()")),
        // The table under another name.
        (
            "SELECT d.id, score() FROM docs d WHERE d.year > 2000 ORDER BY rank_profile('phased') LIMIT 5".into(),
            "WITH top AS (SELECT * FROM docs d WHERE d.year > 2000 ORDER BY bm25(body, :t) DESC LIMIT 100) SELECT d.id, score() FROM top d ORDER BY similarity(emb, :q) + 0.5 * coalesce(rating, 0) DESC LIMIT 5".into(),
        ),
        (
            "SELECT id, score() FROM docs ORDER BY rank_profile('three') LIMIT 10".into(),
            "WITH a AS (SELECT * FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 200), b AS (SELECT * FROM a ORDER BY coalesce(rating, 0) DESC LIMIT 50)
             SELECT id, score() FROM b ORDER BY minmax(bm25(body, :t)) + minmax(similarity(emb, :q)) DESC LIMIT 10"
                .into(),
        ),
        ("SELECT id, year FROM docs WHERE category = 'c03' ORDER BY rank_profile('oldest') LIMIT 6".into(), "SELECT id, year FROM docs WHERE category = 'c03' ORDER BY year LIMIT 6".into()),
    ];
    for (by, out) in &cases {
        let (a, b) = (query(&db, by, &params), query(&db, out, &params));
        same(by, &a, &b);
        // Its plan is the written-out query's, naming the profile.
        let (pa, pb) = (db.explain(by, &params).unwrap(), db.explain(out, &params).unwrap());
        assert!(pa.features.iter().any(|f| f == RANK_PROFILES), "{by}: {:?}", pa.features);
        let mut unnamed = pa.clone();
        let mut j = unnamed.to_json().unwrap();
        strip_profile(&mut j);
        unnamed = Plan::from_json(&j).unwrap();
        assert_eq!(Plan::new(unnamed.root), pb, "{by}");
        let lines = db.sql(&format!("EXPLAIN {by}"), &params).unwrap();
        let SqlResult::Rows(lines) = lines else { panic!() };
        let text: Vec<String> = lines.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
        assert!(text.iter().any(|l| l.trim_start().starts_with("rank profile: ")), "{by}: {text:?}");
    }
    // The phased profiles plan as one Search with phases.
    let p = db.explain("SELECT id FROM docs ORDER BY rank_profile('three') LIMIT 3", &params).unwrap();
    assert!(p.features.iter().any(|f| f == RANK_PHASES), "{:?}", p.features);
}

/// Removes `profile` from every Search of a plan's JSON, and the feature.
fn strip_profile(j: &mut serde_json::Value) {
    match j {
        serde_json::Value::Object(m) => {
            m.remove("profile");
            if let Some(serde_json::Value::Array(fs)) = m.get_mut("features") {
                fs.retain(|f| f != RANK_PROFILES);
            }
            m.values_mut().for_each(strip_profile);
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(strip_profile),
        _ => {}
    }
}

#[test]
fn profiles_are_kept_with_their_table() {
    let dir = tempfile::tempdir().unwrap();
    let (db, params) = create(&dir.path().join("db.strata"), options());
    sql(&db, PROFILES[0]);
    let none = Params::new();
    refused(&db, PROFILES[0], &none, "has a rank profile named boost already; CREATE OR REPLACE RANK PROFILE replaces it");
    refused(&db, &PROFILES[0].replace("boost", "Boost"), &none, "already");
    // Replaced: the same name, in any case, ranks as the new one.
    sql(&db, "CREATE OR REPLACE RANK PROFILE BOOST ON docs AS SELECT * FROM docs ORDER BY coalesce(rating, 0) DESC");
    let ps = db.storage().rank_profiles("docs").unwrap();
    assert_eq!(ps.len(), 1);
    assert_eq!((ps[0].name.as_str(), ps[0].ranking.as_str()), ("BOOST", "SELECT * FROM docs ORDER BY coalesce(rating, 0) DESC"));
    same("replaced", &query(&db, "SELECT id FROM docs ORDER BY rank_profile('boost') LIMIT 9", &params), &query(&db, "SELECT id FROM docs ORDER BY coalesce(rating, 0) DESC LIMIT 9", &params));

    // Dropped, by name or with its table's.
    sql(&db, "DROP RANK PROFILE boost");
    assert!(db.storage().rank_profiles("docs").unwrap().is_empty());
    refused(&db, "SELECT id FROM docs ORDER BY rank_profile('boost') LIMIT 9", &params, "docs has no rank profile named boost");
    refused(&db, "DROP RANK PROFILE boost", &none, "no rank profile named boost");
    sql(&db, "DROP RANK PROFILE IF EXISTS boost");
    sql(&db, "DROP RANK PROFILE IF EXISTS boost ON docs;");
    sql(&db, "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT, weight FLOAT)");
    sql(&db, "INSERT INTO notes (id, body, weight) VALUES (1, 'a cat', 0.5), (2, 'a dog', 0.9), (3, 'cats and dogs', 0.1)");
    sql(&db, "CREATE RANK PROFILE top ON notes AS SELECT * FROM notes ORDER BY weight DESC");
    sql(&db, "CREATE RANK PROFILE top ON docs AS SELECT * FROM docs ORDER BY year DESC");
    refused(&db, "DROP RANK PROFILE top", &none, "docs and notes have a rank profile named top: DROP RANK PROFILE top ON table");
    let ids = |rs: ResultSet| rs.rows.into_iter().map(|r| r[0].clone()).collect::<Vec<_>>();
    assert_eq!(ids(query(&db, "SELECT id FROM notes ORDER BY rank_profile('top') LIMIT 5", &none)), [Value::Int(2), Value::Int(1), Value::Int(3)]);
    sql(&db, "DROP TABLE notes");
    sql(&db, "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT, weight FLOAT)");
    assert!(db.storage().rank_profiles("notes").unwrap().is_empty(), "a table's profiles go with it");
    sql(&db, "DROP RANK PROFILE top");
    assert!(db.storage().rank_profiles("docs").unwrap().is_empty());

    // What a profile's ranking may be.
    for (text, says) in [
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs WHERE year > 2000 ORDER BY year DESC", "it filters (WHERE)"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY year DESC LIMIT 10", "its last ranking has a LIMIT"),
        ("CREATE RANK PROFILE p ON docs AS SELECT id FROM docs ORDER BY year DESC", "selects other than *"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs", "ranks by one expression"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY year, rating", "ranks by one expression"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM notes ORDER BY weight DESC", "reads notes, not docs"),
        ("CREATE RANK PROFILE p ON docs AS WITH a AS (SELECT * FROM docs ORDER BY year DESC) SELECT * FROM a ORDER BY rating DESC", "a phase before the last has no LIMIT"),
        ("CREATE RANK PROFILE p ON docs AS WITH a AS (SELECT * FROM docs ORDER BY year DESC LIMIT 9) SELECT * FROM a ORDER BY rating", "a rerank phase ranks best first (DESC)"),
        ("CREATE RANK PROFILE p ON docs AS WITH a AS (SELECT * FROM docs ORDER BY year DESC LIMIT 9) SELECT * FROM docs ORDER BY rating DESC", "reads docs, not a"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs GROUP BY year ORDER BY year DESC", "it groups"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY rank_profile('q') DESC", "ranks by a rank profile"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY bm25(body, :t) DESC WITH (w => 0.2)", "WITH (w => ...): the ranking reads no parameter :w"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY bm25(body, :t) DESC WITH (t => year)", "each default is parameter => value"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY bm25(body, :t) DESC WITH (t => 'a', t => 'b')", "gives t twice"),
        ("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY year DESC WITH (t => 1) LIMIT 3", "WITH (...) after the ranking ends the statement"),
        ("CREATE RANK PROFILE p ON nothing AS SELECT * FROM nothing ORDER BY year DESC", "no such table: nothing"),
        ("CREATE RANK PROFILE p ON docs", "the form is CREATE [OR REPLACE] RANK PROFILE"),
        ("CREATE RANK PROFILE p ON docs AS", "the profile's ranking is missing"),
        ("CREATE RANK PROFILE \"my p\" ON docs AS SELECT * FROM docs ORDER BY year DESC", "bad rank profile name \"my p\""),
        ("DROP RANK PROFILE p ON docs CASCADE", "the form is DROP RANK PROFILE [IF EXISTS] name [ON table]"),
    ] {
        refused(&db, text, &none, says);
    }
    assert!(db.storage().rank_profiles("docs").unwrap().is_empty());

    // How a query may rank by one.
    sql(&db, PROFILES[0]);
    for (text, says) in [
        ("SELECT id FROM docs ORDER BY rank_profile('nope') LIMIT 3", "docs has no rank profile named nope"),
        ("SELECT id FROM notes ORDER BY rank_profile('boost') LIMIT 3", "notes has no rank profile named boost"),
        ("SELECT id FROM docs ORDER BY rank_profile('boost') ASC LIMIT 3", "ranks best first"),
        ("SELECT id FROM docs ORDER BY rank_profile('boost') + 1 DESC LIMIT 3", "is the whole ORDER BY of a query of one table"),
        ("SELECT id FROM docs ORDER BY rank_profile('boost'), year LIMIT 3", "is the whole ORDER BY"),
        ("SELECT rank_profile('boost') FROM docs LIMIT 3", "is the whole ORDER BY"),
        ("SELECT year, count(*) FROM docs GROUP BY year ORDER BY rank_profile('boost') LIMIT 3", "is the whole ORDER BY"),
        ("WITH a AS (SELECT * FROM docs LIMIT 9) SELECT id FROM a ORDER BY rank_profile('boost') LIMIT 3", "is the whole ORDER BY"),
        ("SELECT id FROM docs WHERE id IN (SELECT id FROM docs ORDER BY rank_profile('boost') LIMIT 3) LIMIT 3", "is the whole ORDER BY"),
        ("SELECT id FROM docs ORDER BY rank_profile('boost', x => 1) LIMIT 3", "rank profile boost reads no parameter :x; it reads :t, :w"),
        ("SELECT id FROM docs ORDER BY rank_profile('boost', w => rating) LIMIT 3", "an argument is a value or a parameter of the query"),
        ("SELECT id FROM docs ORDER BY rank_profile('boost', 0.3) LIMIT 3", "its parameters are named"),
        ("SELECT id FROM docs ORDER BY rank_profile(boost) LIMIT 3", "is the whole ORDER BY"),
    ] {
        refused(&db, text, &params, says);
    }
    // A parameter neither the call nor the profile sets is the query's.
    refused(&db, "SELECT id FROM docs ORDER BY rank_profile('boost') LIMIT 3", &none, "unbound parameter :t");
}

/// A Db of one small table, `t (id, n, body)`.
fn small(path: &Path, opts: DbOptions) -> Db {
    let db = Db::create(path, strata::Schema::new("t", vec![strata::Field::new("id", strata::FieldType::Int), strata::Field::new("n", strata::FieldType::Int), strata::Field::new("body", strata::FieldType::Text { positions: true })], Some("id")).unwrap(), opts).unwrap();
    for i in 0..20 {
        sql(&db, &format!("INSERT INTO t (id, n, body) VALUES ({i}, {}, 'row {i} of cats and dogs')", (i * 7) % 11));
    }
    db
}

fn names(db: &Db) -> Vec<(String, String)> {
    db.storage().rank_profiles("t").unwrap().into_iter().map(|p| (p.name, p.ranking)).collect()
}

/// The first bytes of the database's log.
fn wal_header(path: &Path) -> Vec<u8> {
    let p = if path.is_dir() { path.join("wal") } else { path.with_file_name(format!("{}-wal", path.file_name().unwrap().to_str().unwrap())) };
    let mut b = std::fs::read(p).unwrap();
    b.truncate(40);
    b
}

#[test]
fn profiles_are_logged_and_replayed() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(if layout == Layout::Directory { "db.dir" } else { "db.strata" });
        let opts = DbOptions { layout, sync: true, ..options() };
        let db = small(&path, opts.clone());
        // Rows in the memtable: the first profile flushes them, then starts
        // the log again naming the feature.
        assert_eq!(&wal_header(&path)[..8], b"STRWAL01");
        sql(&db, "CREATE RANK PROFILE by_n ON t AS SELECT * FROM t ORDER BY n DESC");
        let header = wal_header(&path);
        assert_eq!(&header[..8], b"STRWAL02");
        assert_eq!(&header[16..33], b"\x0d\0\0\0rank_profiles");
        sql(&db, "INSERT INTO t (id, n, body) VALUES (100, 50, 'a new row')");
        sql(&db, "CREATE RANK PROFILE by_bm25 ON t AS SELECT * FROM t ORDER BY bm25(body, :w) DESC WITH (w => 'cats')");
        sql(&db, "CREATE OR REPLACE RANK PROFILE by_n ON t AS SELECT * FROM t ORDER BY n");
        let want = names(&db);
        assert_eq!(want.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(), ["by_bm25", "by_n"]);
        let rows = query(&db, "SELECT id FROM t ORDER BY rank_profile('by_n') LIMIT 30", &Params::new()).rows;
        // Without a flush: the profiles are in the log alone.
        drop(db);
        let db = Db::open(&path, opts.clone()).unwrap();
        assert_eq!(names(&db), want, "{layout:?}: replayed");
        assert_eq!(query(&db, "SELECT id FROM t ORDER BY rank_profile('by_n') LIMIT 30", &Params::new()).rows, rows);
        // Flushed: in the manifest, which then needs the feature.
        sql(&db, "DROP RANK PROFILE by_bm25");
        sql(&db, "INSERT INTO t (id, n, body) VALUES (101, -1, 'another row')");
        db.storage().flush().unwrap();
        if layout == Layout::Directory {
            let (_, m) = strata::storage::dir::read_current(&path).unwrap();
            assert_eq!(m.features(), [RANK_PROFILES]);
            assert_eq!(m.tables[0].profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["by_n"]);
        }
        drop(db);
        let db = Db::open(&path, opts.clone()).unwrap();
        assert_eq!(names(&db), [want[1].clone()]);
        // A merge, a checkpoint and vacuuming keep them.
        db.storage().merge(&strata::storage::MergePolicy { merge_factor: 2, floor_docs: 1_000, ..Default::default() }).unwrap();
        if layout == Layout::SingleFile {
            db.storage().vacuum().unwrap();
        }
        sql(&db, "CREATE RANK PROFILE again ON t AS SELECT * FROM t ORDER BY id DESC");
        let to = dir.path().join(if layout == Layout::Directory { "copy.dir" } else { "copy.strata" });
        db.storage().checkpoint(&to, &CheckpointOptions { layout, ..CheckpointOptions::default() }).unwrap();
        let copy = Db::open(&to, opts.clone()).unwrap();
        assert_eq!(names(&copy), names(&db), "{layout:?}: the checkpoint");
        drop(db);
        let db = Db::open(&path, opts).unwrap();
        assert_eq!(names(&db), names(&copy));
    }
}

fn numbered(path: &Path) -> Db {
    let db = small(path, DbOptions { ship_log_bytes: 1 << 24, sync: false, ..options() });
    db.storage().start_seq_no().unwrap();
    db
}

/// Ships what `replica` lacks of `primary`'s log.
fn ship(primary: &Db, replica: &Db) {
    let at = replica.storage().numbering().unwrap().last();
    let LogRead::Txns(got) = primary.storage().log_from(at.seq + 1, 1 << 24) else { panic!("gone") };
    let bytes: Vec<&[u8]> = got.iter().map(|t| &*t.bytes).collect();
    let end = got.last().map_or(at, |t| t.seq);
    assert_eq!(replica.storage().apply_shipped(at, &bytes).unwrap(), Applied::At(end));
}

#[test]
fn a_replica_receives_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let primary = numbered(&dir.path().join("p.strata"));
    // Both start from the same rows, number 0.
    let replica = numbered(&dir.path().join("r.strata"));
    sql(&primary, "INSERT INTO t (id, n, body) VALUES (50, 3, 'cats')");
    sql(&primary, "CREATE RANK PROFILE by_n ON t AS SELECT * FROM t ORDER BY n DESC");
    sql(&primary, "CREATE RANK PROFILE by_bm25 ON t AS SELECT * FROM t ORDER BY bm25(body, :w) DESC WITH (w => 'cats')");
    sql(&primary, "UPDATE t SET n = 99 WHERE id = 3");
    let n = primary.storage().numbering().unwrap().last().seq;
    ship(&primary, &replica);
    assert_eq!(replica.storage().numbering().unwrap().last().seq, n, "each profile is a numbered transaction");
    assert_eq!(names(&replica), names(&primary));
    let q = "SELECT id, score() FROM t ORDER BY rank_profile('by_bm25') LIMIT 30";
    same(q, &query(&replica, q, &Params::new()), &query(&primary, q, &Params::new()));
    sql(&primary, "CREATE OR REPLACE RANK PROFILE by_n ON t AS SELECT * FROM t ORDER BY n");
    sql(&primary, "DROP RANK PROFILE by_bm25");
    ship(&primary, &replica);
    assert_eq!(names(&replica), names(&primary));
    assert_eq!(names(&replica).len(), 1);
    // A plan the primary made runs on the replica.
    let plan = primary.explain("SELECT id FROM t ORDER BY rank_profile('by_n') LIMIT 5", &Params::new()).unwrap();
    assert!(matches!(replica.run(&plan).unwrap(), SqlResult::Rows(rs) if rs.rows.len() == 5));
    // A replica that installs a copy takes the copy's profiles.
    let far = numbered(&dir.path().join("far.strata"));
    sql(&far, "CREATE RANK PROFILE stale ON t AS SELECT * FROM t ORDER BY id DESC");
    let to = dir.path().join("copy.strata");
    primary.storage().checkpoint(&to, &CheckpointOptions::default()).unwrap();
    let copy = Db::open(&to, DbOptions::default()).unwrap();
    sql(&copy, "CREATE RANK PROFILE logged ON t AS SELECT * FROM t ORDER BY id");
    far.storage().install_copy(copy.storage()).unwrap();
    assert_eq!(names(&far), names(&copy));
    assert_eq!(names(&far).iter().map(|p| p.0.as_str()).collect::<Vec<_>>(), ["by_n", "logged"]);
    drop(far);
    let far = Db::open(&dir.path().join("far.strata"), DbOptions::default()).unwrap();
    assert_eq!(names(&far), names(&copy), "installed durably");
}

#[test]
fn a_plan_names_its_profile() {
    let dir = tempfile::tempdir().unwrap();
    let (db, params) = create(&dir.path().join("db.strata"), options());
    sql(&db, PROFILES[1]);
    let q = "SELECT id, score() FROM docs ORDER BY rank_profile('phased') LIMIT 10";
    let plan = db.explain(q, &params).unwrap();
    let hash = db.storage().rank_profiles("docs").unwrap()[0].hash();
    assert_eq!(hash.len(), 16);
    let j = plan.to_json().unwrap();
    let root = j["root"].to_string();
    assert_eq!(root.matches(r#""profile":"#).count(), 1, "{root}");
    assert!(root.contains(&format!(r#""hash":"{hash}""#)) && root.contains(r#""name":"phased""#), "{root}");
    assert!(plan.features.iter().any(|f| f == RANK_PROFILES));
    assert_eq!(Plan::from_json(&j).unwrap(), plan);
    // A reader without the feature refuses it by name.
    let older: Vec<&str> = FEATURES.iter().copied().filter(|f| *f != RANK_PROFILES).collect();
    let fs: Vec<&str> = plan.features.iter().map(String::as_str).collect();
    let e = Plan::readable_by(plan.version as u64, &fs, &older).unwrap_err().to_string();
    assert!(e.contains(r#"feature "rank_profiles""#), "{e}");
    let rows = |r: SqlResult| match r {
        SqlResult::Rows(rs) => rs,
        _ => panic!(),
    };
    let before = rows(db.run(&plan).unwrap());
    same("the plan", &before, &query(&db, q, &params));
    // Another definition under the name: the plan is refused.
    sql(&db, &PROFILES[1].replace("CREATE", "CREATE OR REPLACE").replace("0.5", "0.6"));
    let e = db.run(&plan).err().unwrap().to_string();
    assert!(e.contains("rank profile phased of docs is not the one the plan was made with") && e.contains(&hash), "{e}");
    // The same definition again: it runs again.
    sql(&db, &PROFILES[1].replace("CREATE", "CREATE OR REPLACE"));
    same("the same definition", &rows(db.run(&plan).unwrap()), &before);
    sql(&db, "DROP RANK PROFILE phased");
    let e = db.run(&plan).err().unwrap().to_string();
    assert!(e.contains("docs has no rank profile phased, which the plan ranks as"), "{e}");
    // Hashes name definitions: the name, the ranking and the defaults.
    let p = RankProfile { name: "a".into(), ranking: "SELECT * FROM docs ORDER BY year".into(), defaults: Default::default() };
    let mut other = p.clone();
    other.defaults.insert("w".into(), serde_json::json!(1));
    assert_ne!(p.hash(), other.hash());
    assert_eq!(p.hash(), p.clone().hash());
}
