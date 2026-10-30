//! The format 2 and format 3 fixtures (`tests/fixtures/README.md`), which
//! older versions of Strata wrote, open with this one in both layouts:
//! their rows, filters and searches are as written, they take writes,
//! flushes and merges and stay in the format they were written in. So do
//! the fixtures of numbered writes, which go on counting from theirs, and
//! those of rank profiles, which rank as they were written to until the
//! last is dropped. And this version still writes the same bytes for the
//! same writes, so a
//! change to what Strata writes is a choice, made with a new fixture set.

#[path = "fixtures/writer.rs"]
mod writer;

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::storage::pager::{Meta, PAGE};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::Value;
use writer::OCT1;

const DAY: i64 = 86_400_000_000;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The fixture sets of one format: `format3`, which this version writes,
/// and any older set kept as `format3-<commit>`.
fn sets(format: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir() && p.file_name().unwrap().to_str().unwrap().split('-').next() == Some(format))
        .collect();
    v.sort();
    assert!(!v.is_empty(), "no {format} fixtures");
    v
}

/// Copies of a set's two databases (with the single file's WAL) in `dir`.
fn copies(set: &Path, dir: &Path) -> [PathBuf; 2] {
    std::fs::copy(set.join("db.strata"), dir.join("db.strata")).unwrap();
    std::fs::copy(set.join("db.strata-wal"), dir.join("db.strata-wal")).unwrap();
    std::fs::create_dir(dir.join("db.dir")).unwrap();
    for e in std::fs::read_dir(set.join("db.dir")).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), dir.join("db.dir").join(e.file_name())).unwrap();
    }
    [dir.join("db.strata"), dir.join("db.dir")]
}

fn options(path: &Path) -> DbOptions {
    let layout = if path.is_dir() { Layout::Directory } else { Layout::SingleFile };
    DbOptions { layout, sync: false, ..DbOptions::default() }
}

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    let SqlResult::Rows(r) = run(db, sql) else { panic!("{sql}: expected rows") };
    r.rows
}

/// The first column of what `sql` returns, as text, sorted.
fn firsts(db: &Db, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = rows(db, sql)
        .into_iter()
        .map(|r| match &r[0] {
            Value::Str(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            v => panic!("{sql}: {v:?}"),
        })
        .collect();
    v.sort();
    v
}

fn key(i: u64) -> String {
    format!("k{i:06}")
}

/// The format of the newest committed version on disk.
fn format_on_disk(path: &Path) -> u32 {
    if path.is_dir() {
        let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
        return cur.get("format").map_or(1, |f| f.as_u64().unwrap() as u32);
    }
    meta_slots(path).into_iter().flatten().max_by_key(|m| m.txn).unwrap().format
}

fn meta_slots(path: &Path) -> Vec<Option<Meta>> {
    let f = File::open(path).unwrap();
    (0..2u64)
        .map(|slot| {
            let mut page = vec![0u8; PAGE as usize];
            f.read_exact_at(&mut page, slot * PAGE).unwrap();
            Meta::decode(&page).unwrap()
        })
        .collect()
}

fn merge(db: &Db) {
    let stats = db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
    assert!(stats.docs_merged > 0, "{stats:?}");
}

/// What the format 2 fixtures hold: table `t` by key number, with `n` and
/// the version its body was written with, and table `u` by id, with the
/// key number of its parent.
#[derive(Clone)]
struct Format2 {
    t: BTreeMap<u64, (i64, i64)>,
    u: BTreeMap<i64, u64>,
}

impl Format2 {
    /// As `writer::format2` left it.
    fn written() -> Format2 {
        let mut t: BTreeMap<u64, (i64, i64)> = (5..30).map(|i| (i, (0, 0))).chain((30..40).map(|i| (i, (1, 1)))).collect();
        for i in 5..8 {
            t.get_mut(&i).unwrap().0 = 7;
        }
        t.remove(&10);
        t.insert(11, (2, 2));
        let u = (0..9).chain([10]).map(|i| (i, i as u64 % 7)).collect();
        Format2 { t, u }
    }

    fn check(&self, db: &Db) {
        let keys = |f: &dyn Fn(&(i64, i64)) -> bool| self.t.iter().filter(|(_, r)| f(r)).map(|(&i, _)| key(i)).collect::<Vec<_>>();
        assert_eq!(firsts(db, "SELECT id FROM t LIMIT 1000"), keys(&|_| true));
        for n in [0, 1, 2, 7, 9] {
            assert_eq!(firsts(db, &format!("SELECT id FROM t WHERE n = {n} LIMIT 1000")), keys(&|r| r.0 == n), "n = {n}");
        }
        // Bodies say "document {i} version {v}", and documents 1 and 2 are
        // deleted.
        for v in [1, 2] {
            assert_eq!(firsts(db, &format!("SELECT id FROM t WHERE MATCH(body, '{v}') LIMIT 1000")), keys(&|r| r.1 == v), "version {v}");
        }
        let ids = |f: &dyn Fn(u64) -> bool| {
            let mut v: Vec<String> = self.u.iter().filter(|(_, &p)| f(p)).map(|(i, _)| i.to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(firsts(db, "SELECT id FROM u LIMIT 1000"), ids(&|_| true));
        assert_eq!(firsts(db, "SELECT id FROM u WHERE parent = 'k000003' LIMIT 1000"), ids(&|p| p == 3));
        // english stems "running".
        assert_eq!(firsts(db, "SELECT id FROM u WHERE MATCH(note, 'run') LIMIT 1000"), ids(&|_| true));
    }
}

#[test]
fn format_2_fixtures_open_and_stay_format_2() {
    for set in sets("format2") {
        let dir = tempfile::tempdir().unwrap();
        for path in copies(&set, dir.path()) {
            let what = path.display().to_string();
            let mut model = Format2::written();
            let db = Db::open(&path, options(&path)).unwrap_or_else(|e| panic!("{what}: {e}"));
            model.check(&db);

            run(&db, "INSERT INTO t (id, n, body) VALUES ('k000100', 9, 'document 100 version 1 lorem ipsum')");
            run(&db, "UPDATE t SET n = 9 WHERE id = 'k000012'");
            run(&db, "DELETE FROM t WHERE id = 'k000013'");
            run(&db, "INSERT INTO u (id, parent, note) VALUES (11, 'k000003', 'note 11 about the running cats')");
            run(&db, "DELETE FROM u WHERE id = 0");
            model.t.insert(100, (9, 1));
            model.t.get_mut(&12).unwrap().0 = 9;
            model.t.remove(&13);
            model.u.insert(11, 3);
            model.u.remove(&0);
            model.check(&db);
            db.storage().flush().unwrap();
            merge(&db);
            db.storage().check().unwrap();
            model.check(&db);
            assert_eq!(format_on_disk(&path), 2, "{what}");
            drop(db);

            let db = Db::open(&path, options(&path)).unwrap();
            model.check(&db);
        }
    }
}

/// A row of the format 3 fixtures' `tasks` table.
#[derive(Clone)]
struct Task {
    /// Which of the writer's four titles (and bodies) it has.
    title: u64,
    done: bool,
    due: Option<i64>,
    priority: i64,
    open: bool,
}

/// What the format 3 fixtures hold, by key number.
#[derive(Clone)]
struct Format3(BTreeMap<u64, Task>);

impl Format3 {
    /// As `writer::format3` left it.
    fn written() -> Format3 {
        let task = |i: u64, priority| Task { title: i % 4, done: i % 3 == 0, due: (i % 5 != 4).then(|| OCT1 + i as i64 * DAY), priority, open: i % 2 == 0 };
        let mut m: BTreeMap<u64, Task> = (5..30).map(|i| (i, task(i, 2))).chain((30..40).map(|i| (i, task(i, 1)))).collect();
        for i in 5..8 {
            m.get_mut(&i).unwrap().priority = 7;
        }
        m.remove(&10);
        m.insert(11, task(11, 3));
        Format3(m)
    }

    fn check(&self, db: &Db) {
        let keys = |f: &dyn Fn(&Task) -> bool| self.0.iter().filter(|(_, t)| f(t)).map(|(&i, _)| key(i)).collect::<Vec<_>>();
        let q = |cond: &str| firsts(db, &format!("SELECT id FROM tasks WHERE {cond} LIMIT 1000"));
        assert_eq!(firsts(db, "SELECT id FROM tasks LIMIT 1000"), keys(&|_| true));
        assert_eq!(q("done"), keys(&|t| t.done));
        assert_eq!(q("done IS NOT TRUE"), keys(&|t| !t.done));
        assert_eq!(q("due IS NULL"), keys(&|t| t.due.is_none()));
        assert_eq!(q("due < '2026-10-08'"), keys(&|t| t.due.is_some_and(|d| d < OCT1 + 7 * DAY)));
        assert_eq!(q("due BETWEEN '2026-10-02' AND '2026-10-03T00:00:00Z'"), keys(&|t| t.due.is_some_and(|d| (OCT1 + DAY..=OCT1 + 2 * DAY).contains(&d))));
        for p in [1, 2, 3, 7] {
            assert_eq!(q(&format!("priority = {p}")), keys(&|t| t.priority == p), "priority = {p}");
        }
        assert_eq!(q("state = 'open'"), keys(&|t| t.open));
        // Titles are analysed by simple, which keeps words whole, and bodies
        // by english, which stems and drops stopwords. The four titles are
        // "The Running Cats", "they run", "Über alles" and "a cat sat".
        assert_eq!(q("MATCH(title, 'run')"), keys(&|t| t.title == 1));
        assert_eq!(q("MATCH(body, 'run')"), keys(&|t| t.title <= 1));
        assert_eq!(q("MATCH(title, 'the')"), keys(&|t| t.title == 0));
        assert_eq!(q("MATCH(body, 'the')"), Vec::<String>::new());
        assert_eq!(q("MATCH(title, 'cat')"), keys(&|t| t.title == 3));
        assert_eq!(q("MATCH(body, 'cat')"), keys(&|t| t.title == 0 || t.title == 3));
        assert_eq!(q("MATCH(title, 'uber')"), keys(&|t| t.title == 2));
    }
}

#[test]
fn format_3_fixtures_open_and_stay_format_3() {
    for set in sets("format3") {
        let dir = tempfile::tempdir().unwrap();
        for path in copies(&set, dir.path()) {
            let what = path.display().to_string();
            let mut model = Format3::written();
            let db = Db::open(&path, options(&path)).unwrap_or_else(|e| panic!("{what}: {e}"));
            model.check(&db);

            // done, priority and state take their defaults.
            run(&db, "INSERT INTO tasks (id, title, body, due) VALUES ('k000100', 'they run 100', 'they run 100', '2026-10-02')");
            assert_eq!(
                rows(&db, "SELECT done, priority, state, due FROM tasks WHERE id = 'k000100'"),
                [vec![Value::Bool(false), Value::Int(2), Value::Str("open".into()), Value::Timestamp(OCT1 + DAY)]]
            );
            for bad in ["INSERT INTO tasks (id, priority) VALUES ('k000101', NULL)", "UPDATE tasks SET done = NULL WHERE id = 'k000005'"] {
                let e = db.sql(bad, &Params::new()).err().unwrap_or_else(|| panic!("{bad}: NOT NULL")).to_string();
                assert!(e.contains("NOT NULL"), "{bad}: {e}");
            }
            run(&db, "UPDATE tasks SET done = TRUE, priority = 4 WHERE id = 'k000005'");
            run(&db, "DELETE FROM tasks WHERE id = 'k000006'");
            model.0.insert(100, Task { title: 1, done: false, due: Some(OCT1 + DAY), priority: 2, open: true });
            let t = model.0.get_mut(&5).unwrap();
            (t.done, t.priority) = (true, 4);
            model.0.remove(&6);
            model.check(&db);
            assert_eq!(firsts(&db, "SELECT id FROM tasks WHERE priority = 4"), [key(5)]);
            db.storage().flush().unwrap();
            merge(&db);
            db.storage().check().unwrap();
            model.check(&db);
            assert_eq!(format_on_disk(&path), 3, "{what}");
            drop(db);

            let db = Db::open(&path, options(&path)).unwrap();
            model.check(&db);
        }
    }
}

/// The fixtures of numbered writes hold format 2's table `t`, written
/// under term 2 in five numbered transactions, and each row the number of
/// its last write (0 for rows from before numbering). A write takes the
/// next number, through a flush, a merge and opening again.
#[test]
fn numbered_fixtures_open_and_go_on_counting() {
    use strata::storage::SeqNo;
    for set in sets("seqno") {
        let dir = tempfile::tempdir().unwrap();
        for path in copies(&set, dir.path()) {
            let what = path.display().to_string();
            let t = Format2::written().t;
            let ids = |t: &BTreeMap<u64, (i64, i64)>, n: Option<i64>| t.iter().filter(|(_, r)| n.is_none_or(|n| r.0 == n)).map(|(&i, _)| key(i)).collect::<Vec<_>>();
            let check = |db: &Db, t: &BTreeMap<u64, (i64, i64)>| {
                assert_eq!(firsts(db, "SELECT id FROM t LIMIT 1000"), ids(t, None), "{what}");
                for n in [0, 1, 2, 7, 9] {
                    assert_eq!(firsts(db, &format!("SELECT id FROM t WHERE n = {n} LIMIT 1000")), ids(t, Some(n)), "{what}: n = {n}");
                }
            };
            // Each row's number: the two inserts are 1 and 2, the deletes 3,
            // the values set in place 4, and the transaction left in the WAL 5.
            let mut seqs: BTreeMap<u64, u64> = (5..40).map(|i| (i, [0, 1, 2, 5][i as usize / 10])).collect();
            seqs.extend([(5, 4), (6, 4), (7, 4), (11, 5)]);
            seqs.remove(&10);
            let check_seqs = |db: &Db, seqs: &BTreeMap<u64, u64>| {
                for i in (0..40).chain([100]) {
                    assert_eq!(db.storage().seq_of(None, &Value::Str(key(i))).unwrap(), seqs.get(&i).copied(), "{what}: {}", key(i));
                }
            };
            let db = Db::open(&path, options(&path)).unwrap_or_else(|e| panic!("{what}: {e}"));
            check(&db, &t);
            assert_eq!(db.storage().seq_no(), Some(SeqNo { term: 2, seq: 5 }), "{what}");
            assert_eq!(db.storage().numbering().unwrap().terms, vec![(1, 1), (2, 1)], "{what}");
            check_seqs(&db, &seqs);

            let mut t = t;
            run(&db, "INSERT INTO t (id, n, body) VALUES ('k000100', 9, 'document 100 version 1 lorem ipsum')");
            run(&db, "UPDATE t SET n = 9 WHERE id = 'k000012'");
            t.insert(100, (9, 1));
            t.get_mut(&12).unwrap().0 = 9;
            assert_eq!(db.storage().seq_no(), Some(SeqNo { term: 2, seq: 7 }), "{what}");
            seqs.extend([(100, 6), (12, 7)]);
            check_seqs(&db, &seqs);
            db.storage().flush().unwrap();
            merge(&db);
            db.storage().check().unwrap();
            check(&db, &t);
            check_seqs(&db, &seqs);
            assert_eq!(format_on_disk(&path), 3, "{what}");
            drop(db);

            let db = Db::open(&path, options(&path)).unwrap();
            check(&db, &t);
            assert_eq!(db.storage().seq_no(), Some(SeqNo { term: 2, seq: 7 }), "{what}");
            check_seqs(&db, &seqs);
        }
    }
}

/// The fixtures of rank profiles hold format 2's rows; `t` has `by_n` as
/// replaced in the WAL (its other profile, flushed into the manifest, was
/// dropped there) and `u` has `notes`, logged there too. Profiles change
/// and survive a flush, a merge and opening again; once the last is
/// dropped and flushed, the database is format 2 again and its log names
/// no feature.
#[test]
fn profile_fixtures_rank_as_written() {
    for set in sets("profiles") {
        let dir = tempfile::tempdir().unwrap();
        for path in copies(&set, dir.path()) {
            let what = path.display().to_string();
            let wal = if path.is_dir() { path.join("wal") } else { dir.path().join("db.strata-wal") };
            let header = |wal: &Path| std::fs::read(wal).unwrap()[..33].to_vec();
            assert_eq!(header(&wal)[..8], *b"STRWAL02", "{what}");
            assert_eq!(header(&wal)[16..], *b"\x0d\0\0\0rank_profiles", "{what}");
            assert_eq!(format_on_disk(&path), 3, "{what}");
            if path.is_dir() {
                let (_, m) = strata::storage::dir::read_current(&path).unwrap();
                assert_eq!(m.features(), ["rank_profiles"], "{what}");
                let names: Vec<&str> = m.tables[0].profiles.iter().map(|p| p.name.as_str()).collect();
                assert_eq!(names, ["by_n", "phased"], "{what}: flushed with the deletes");
            }
            let profiles = |db: &Db| {
                ["t", "u"].map(|t| db.storage().rank_profiles(t).unwrap().into_iter().map(|p| format!("{}: {}", p.name, p.ranking)).collect::<Vec<_>>())
            };
            let check = |db: &Db, by_n: &str| {
                Format2::written().check(db);
                assert_eq!(rows(db, "SELECT id, n FROM t ORDER BY rank_profile('by_n') LIMIT 1000"), rows(db, &format!("SELECT id, n FROM t ORDER BY {by_n} LIMIT 1000")), "{what}");
                let (a, b) = (rows(db, "SELECT id, score() FROM u ORDER BY rank_profile('notes') LIMIT 5"), rows(db, "SELECT id, score() FROM u ORDER BY bm25(note, 'cats') DESC LIMIT 5"));
                assert!(a.len() == 5 && a == b, "{what}: {a:?}");
            };
            let db = Db::open(&path, options(&path)).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(profiles(&db), [vec!["by_n: SELECT * FROM t ORDER BY n".to_string()], vec!["notes: SELECT * FROM u ORDER BY bm25(note, :q) DESC".to_string()]], "{what}");
            check(&db, "n");
            run(&db, "CREATE OR REPLACE RANK PROFILE by_n ON t AS SELECT * FROM t ORDER BY n DESC");
            run(&db, "CREATE RANK PROFILE recent ON t AS WITH a AS (SELECT * FROM t ORDER BY bm25(body, :q) DESC LIMIT 10) SELECT * FROM a ORDER BY n DESC WITH (q => 'version')");
            db.storage().flush().unwrap();
            merge(&db);
            db.storage().check().unwrap();
            check(&db, "n DESC");
            assert_eq!(format_on_disk(&path), 3, "{what}");
            drop(db);

            let db = Db::open(&path, options(&path)).unwrap();
            check(&db, "n DESC");
            let [t, u] = profiles(&db);
            assert_eq!((t.len(), u.len()), (2, 1), "{what}: {t:?}");
            let recent = "SELECT id FROM t ORDER BY rank_profile('recent') LIMIT 5";
            let written = "WITH a AS (SELECT * FROM t ORDER BY bm25(body, 'version') DESC LIMIT 10) SELECT id FROM a ORDER BY n DESC LIMIT 5";
            assert_eq!(rows(&db, recent), rows(&db, written), "{what}");
            for (p, t) in [("by_n", "t"), ("recent", "t"), ("notes", "u")] {
                run(&db, &format!("DROP RANK PROFILE {p} ON {t}"));
            }
            db.storage().flush().unwrap();
            assert_eq!(format_on_disk(&path), 2, "{what}: no table has a profile");
            assert_eq!(std::fs::read(&wal).unwrap()[..8], *b"STRWAL01", "{what}");
            drop(db);
            let db = Db::open(&path, options(&path)).unwrap();
            Format2::written().check(&db);
            assert_eq!(profiles(&db), [Vec::<String>::new(), Vec::new()], "{what}");
        }
    }
}

/// `new` holds the bytes of `old` but for the database id: in a WAL's
/// header, and in a single file's two meta pages.
fn same_bytes(old: &Path, new: &Path) {
    let (a, b) = (std::fs::read(old).unwrap(), std::fs::read(new).unwrap());
    let name = old.file_name().unwrap().to_str().unwrap();
    let skip = match name {
        "wal" | "db.strata-wal" => {
            assert_eq!(a[..8], b[..8], "{name}: header");
            16
        }
        "db.strata" => {
            let unid = |p: &Path| meta_slots(p).into_iter().map(|m| m.map(|m| Meta { db_id: 0, ..m })).collect::<Vec<_>>();
            assert_eq!(unid(old), unid(new), "{name}: meta pages");
            2 * PAGE as usize
        }
        _ => 0,
    };
    let at = a[skip..].iter().zip(&b[skip..]).position(|(x, y)| x != y);
    assert!(
        a.len() == b.len() && at.is_none(),
        "{} is not what Strata writes now ({} bytes against {}, first difference at byte {:?}): if the change is deliberate, keep this set and write a new one as tests/fixtures/README.md says",
        old.display(),
        a.len(),
        b.len(),
        at.map(|p| p + skip)
    );
}

#[test]
fn this_version_writes_the_fixtures_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    writer::write_all(dir.path());
    for set in ["format2", "format3", "seqno", "profiles"] {
        let (old, new) = (fixtures().join(set), dir.path().join(set));
        same_bytes(&old.join("db.strata"), &new.join("db.strata"));
        same_bytes(&old.join("db.strata-wal"), &new.join("db.strata-wal"));
        let names = |d: &Path| {
            let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            v.sort();
            v
        };
        let (old, new) = (old.join("db.dir"), new.join("db.dir"));
        assert_eq!(names(&old), names(&new), "{set}");
        for name in names(&old) {
            match name.as_str() {
                "LOCK" => {}
                "CURRENT" => {
                    let unid = |p: &Path| {
                        let mut cur: serde_json::Value = serde_json::from_slice(&std::fs::read(p.join("CURRENT")).unwrap()).unwrap();
                        cur.as_object_mut().unwrap().remove("db_id").expect("a db_id");
                        cur
                    };
                    assert_eq!(unid(&old), unid(&new), "{set}");
                }
                _ => same_bytes(&old.join(&name), &new.join(&name)),
            }
        }
    }
}
