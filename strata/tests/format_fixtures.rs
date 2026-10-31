//! The fixture sets (`tests/fixtures/README.md`). Those that versions of
//! Strata from before field ids wrote (format 2 and format 3, numbered
//! writes, rank profiles) are refused by a plain open, which names
//! `strata::upgrade`; opened to be copied (`DbOptions::open_old`), their
//! rows, filters and searches are as written, they take writes, flushes
//! and merges and stay in the format they were written in, the numbered
//! ones go on counting from theirs and the profiles rank as they were
//! written to until the last is dropped. `strata::upgrade` copies each
//! into a database of this version that holds the same, but the numbered
//! ones, which it refuses. The sets this version wrote (`fieldids/`, the
//! same writes) open as any database does and pass the same checks. And
//! this version still writes their bytes for the same writes, so a change
//! to what Strata writes is a choice, made with a new fixture set.

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
use strata::upgrade::{needs_upgrade, old_path, upgrade};
use strata::value::Value;
use writer::OCT1;

const DAY: i64 = 86_400_000_000;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Whether a set was written before field ids, or by this version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Era {
    /// Opens only to be copied (`DbOptions::open_old`), and stays in the
    /// format it was written in.
    Old,
    /// Format 3 with `field_ids`.
    Ids,
}

/// The sets of one kind of writes (`format2`, `format3`, `seqno`,
/// `profiles`): those written before field ids, at the top (and any older
/// copy kept as `<name>-<commit>`), and those under `fieldids/` (and any
/// older `fieldids-<commit>/`).
fn sets(name: &str) -> Vec<(PathBuf, Era)> {
    let first = |p: &Path| p.file_name().unwrap().to_str().unwrap().split('-').next().unwrap().to_string();
    let mut v: Vec<(PathBuf, Era)> = Vec::new();
    for e in std::fs::read_dir(fixtures()).unwrap() {
        let p = e.unwrap().path();
        if !p.is_dir() {
            continue;
        }
        if first(&p) == name {
            v.push((p, Era::Old));
        } else if first(&p) == "fieldids" && p.join(name).is_dir() {
            v.push((p.join(name), Era::Ids));
        }
    }
    v.sort();
    assert!(v.iter().any(|s| s.1 == Era::Old) && v.iter().any(|s| s.1 == Era::Ids), "{name}: {v:?}");
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

fn options(path: &Path, era: Era) -> DbOptions {
    let layout = if path.is_dir() { Layout::Directory } else { Layout::SingleFile };
    DbOptions { layout, sync: false, open_old: era == Era::Old, ..DbOptions::default() }
}

/// Opens a set's copy as its era opens; a set from before field ids is
/// refused without `open_old`, by a message that names the upgrade.
fn open(path: &Path, era: Era) -> Db {
    let what = path.display();
    if era == Era::Old {
        let e = Db::open(path, options(path, Era::Ids)).err().unwrap_or_else(|| panic!("{what}: opened without open_old")).to_string();
        assert!(e.contains("before field ids") && e.contains("strata::upgrade"), "{what}: {e}");
    }
    assert_eq!(needs_upgrade(path).unwrap(), era == Era::Old, "{what}");
    Db::open(path, options(path, era)).unwrap_or_else(|e| panic!("{what}: {e}"))
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
fn format_2_fixtures_open_and_stay_in_their_format() {
    for (set, era) in sets("format2") {
        let dir = tempfile::tempdir().unwrap();
        for path in copies(&set, dir.path()) {
            let what = path.display().to_string();
            let mut model = Format2::written();
            let db = open(&path, era);
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
            assert_eq!(format_on_disk(&path), if era == Era::Old { 2 } else { 3 }, "{what}");
            drop(db);

            let db = open(&path, era);
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
    for (set, era) in sets("format3") {
        let dir = tempfile::tempdir().unwrap();
        for path in copies(&set, dir.path()) {
            let what = path.display().to_string();
            let mut model = Format3::written();
            let db = open(&path, era);
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

            let db = open(&path, era);
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
    for (set, era) in sets("seqno") {
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
            let db = open(&path, era);
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

            let db = open(&path, era);
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
/// dropped and flushed, a set from before field ids is format 2 again
/// (one of this version stays format 3, for its ids), and its log names no
/// feature.
#[test]
fn profile_fixtures_rank_as_written() {
    for (set, era) in sets("profiles") {
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
                let features: &[&str] = if era == Era::Old { &["rank_profiles"] } else { &["field_ids", "rank_profiles"] };
                assert_eq!(m.features(), features, "{what}");
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
            let db = open(&path, era);
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

            let db = open(&path, era);
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
            assert_eq!(format_on_disk(&path), if era == Era::Old { 2 } else { 3 }, "{what}: no table has a profile");
            assert_eq!(std::fs::read(&wal).unwrap()[..8], *b"STRWAL01", "{what}");
            drop(db);
            let db = open(&path, era);
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

/// The sets this version writes, by the writes in `writer.rs`, are those in
/// `fieldids/`.
#[test]
fn this_version_writes_the_fixtures_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    writer::write_all(dir.path());
    for set in ["format2", "format3", "seqno", "profiles"] {
        let (old, new) = (fixtures().join("fieldids").join(set), dir.path().join(set));
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

/// `strata::upgrade` copies a set from before field ids into a database of
/// this version, in the same layout, that holds the same rows, filters,
/// searches and rank profiles and takes writes; it keeps the old database
/// beside it, which still opens to be copied, and a second call does
/// nothing.
#[test]
fn upgrade_copies_the_sets_from_before_field_ids() {
    for name in ["format2", "format3", "profiles"] {
        for (set, era) in sets(name) {
            if era != Era::Old {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            for path in copies(&set, dir.path()) {
                let what = path.display().to_string();
                let before = {
                    let db = open(&path, Era::Old);
                    db.storage().tables().iter().map(|t| (t.table.clone(), db.storage().rank_profiles(&t.table).unwrap())).collect::<Vec<_>>()
                };
                let up = upgrade(&path).unwrap().unwrap_or_else(|| panic!("{what}: nothing to upgrade"));
                assert_eq!(up.old, old_path(&path), "{what}");
                assert_eq!(up.tables, before.len(), "{what}");
                assert_eq!(path.is_dir(), up.old.is_dir(), "{what}: the same layout");
                assert_eq!(upgrade(&path).unwrap(), None, "{what}: upgraded already");

                let db = open(&path, Era::Ids);
                assert_eq!(format_on_disk(&path), 3, "{what}");
                let after: Vec<_> = db.storage().tables().iter().map(|t| (t.table.clone(), db.storage().rank_profiles(&t.table).unwrap())).collect();
                assert_eq!(after, before, "{what}: tables and profiles");
                match name {
                    "format3" => {
                        let mut model = Format3::written();
                        model.check(&db);
                        assert_eq!(up.rows, model.0.len() as u64, "{what}");
                        run(&db, "UPDATE tasks SET done = TRUE, priority = 4 WHERE id = 'k000005'");
                        let t = model.0.get_mut(&5).unwrap();
                        (t.done, t.priority) = (true, 4);
                        db.storage().flush().unwrap();
                        model.check(&db);
                    }
                    _ => {
                        let mut model = Format2::written();
                        model.check(&db);
                        assert_eq!(up.rows, (model.t.len() + model.u.len()) as u64, "{what}");
                        // A second segment beside the copy's, to merge.
                        run(&db, "INSERT INTO t (id, n, body) VALUES ('k000100', 9, 'document 100 version 1 lorem ipsum')");
                        model.t.insert(100, (9, 1));
                        db.storage().flush().unwrap();
                        run(&db, "UPDATE t SET n = 9 WHERE id = 'k000012'");
                        run(&db, "DELETE FROM u WHERE id = 0");
                        model.t.get_mut(&12).unwrap().0 = 9;
                        model.u.remove(&0);
                        db.storage().flush().unwrap();
                        merge(&db);
                        model.check(&db);
                    }
                }
                db.storage().check().unwrap();
                drop(db);
                // The old database is as it was: it still needs the upgrade.
                assert!(needs_upgrade(&up.old).unwrap(), "{what}");
                drop(open(&up.old, Era::Old));
            }
        }
    }
}

/// A database that numbers its writes is not copied: its copy could not go
/// on from its numbers. The upgrade leaves it as it was.
#[test]
fn upgrade_refuses_numbered_writes() {
    for (set, era) in sets("seqno") {
        if era != Era::Old {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        for path in copies(&set, dir.path()) {
            let what = path.display().to_string();
            let e = upgrade(&path).unwrap_err().to_string();
            assert!(e.contains("numbers its writes"), "{what}: {e}");
            assert!(!old_path(&path).exists(), "{what}");
            let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            assert!(!names.iter().any(|n| n.contains("upgrading")), "{what}: {names:?}");
            drop(open(&path, Era::Old));
        }
    }
}

/// An upgrade cut short between its two renames (the old database moved
/// aside, the copy not yet in its place) is finished by the next call; one
/// cut short while copying starts again.
#[test]
fn an_upgrade_cut_short_is_finished() {
    let (set, _) = sets("format2").into_iter().find(|s| s.1 == Era::Old).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for path in copies(&set, dir.path()) {
        let what = path.display().to_string();
        let mut upgrading = path.clone().into_os_string();
        upgrading.push(".upgrading");
        let upgrading = PathBuf::from(upgrading);
        // Cut short while copying: a partial copy is left beside it.
        if path.is_dir() {
            std::fs::create_dir(&upgrading).unwrap();
            std::fs::write(upgrading.join("CURRENT"), b"partial").unwrap();
        } else {
            std::fs::write(&upgrading, b"partial").unwrap();
        }
        upgrade(&path).unwrap().unwrap();
        assert!(!upgrading.exists(), "{what}");
        // Cut short between the renames: move the copy back.
        let wal = |p: &Path| {
            let mut w = p.to_path_buf().into_os_string();
            w.push("-wal");
            PathBuf::from(w)
        };
        std::fs::rename(&path, &upgrading).unwrap();
        if wal(&path).exists() {
            std::fs::rename(wal(&path), wal(&upgrading)).unwrap();
        }
        assert!(!path.exists());
        assert!(upgrade(&path).unwrap().is_some(), "{what}");
        assert!(!upgrading.exists(), "{what}");
        let db = open(&path, Era::Ids);
        Format2::written().check(&db);
    }
}
