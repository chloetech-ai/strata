//! Databases with several tables: one transaction over several tables, WAL
//! replay, flushes and merges per table, CREATE and DROP TABLE in both
//! layouts, snapshots across a drop, files written before there were
//! tables, a randomised run against a model, and crash recovery across
//! tables (as in `storage_crash.rs`, whose log format this extends with a
//! table per operation).

mod storage_util;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::{Rng, SeedableRng};
use storage_util::{doc, key, live_keys};
use strata::error::corrupt;
use strata::segment::{DocId, SegmentFile};
use strata::storage::pager::{Meta, PAGE};
use strata::storage::{Database, DbOptions, DocumentSource, Layout, MergePolicy, PkIndex};
use strata::{Document, Error, Result, Schema, Value};

/// The storage tests' schema under another table name.
fn schema(name: &str) -> Schema {
    let mut s = storage_util::schema();
    s.table = name.into();
    s
}

/// A doc store stand-in: the latest document written for each table and key.
#[derive(Default)]
struct Source(Mutex<HashMap<(String, String), Document>>);

impl Source {
    fn put(&self, table: &str, d: &Document) {
        self.0.lock().unwrap().insert((table.to_string(), d[0].as_str().unwrap().to_string()), d.clone());
    }
}

impl DocumentSource for Source {
    fn documents(&self, schema: &Schema, seg: &SegmentFile, docs: &[DocId]) -> Result<Vec<Document>> {
        let pk = PkIndex::open(seg)?.ok_or_else(|| corrupt("no pk"))?;
        let m = self.0.lock().unwrap();
        docs.iter()
            .map(|&d| {
                let k = pk.key(d).as_str().unwrap().to_string();
                m.get(&(schema.table.clone(), k.clone())).cloned().ok_or_else(|| corrupt(format!("source lacks {}.{k}", schema.table)))
            })
            .collect()
    }
}

fn options(src: &Arc<Source>, layout: Layout) -> DbOptions {
    DbOptions { doc_source: Some(src.clone()), sync: false, layout, ..DbOptions::default() }
}

fn layout_of(path: &Path) -> Layout {
    if path.extension().is_some_and(|e| e == "dir") {
        Layout::Directory
    } else {
        Layout::SingleFile
    }
}

/// Every table's live key numbers, by table name. Checks that each key is
/// live once, that counts agree, and that lookups find a table's own rows.
fn state(db: &Database) -> BTreeMap<String, BTreeSet<u64>> {
    let snap = db.snapshot();
    let names: Vec<String> = snap.schemas().map(|s| s.table.clone()).collect();
    let mut out = BTreeMap::new();
    for name in names {
        let t = snap.table(&name).unwrap();
        let live = live_keys(&t);
        assert!(live.values().all(|&c| c == 1), "{name}: a key is live twice");
        assert_eq!(t.n_live_docs(), live.len() as u64, "{name}");
        for k in live.keys().take(20) {
            let (seg, d) = t.lookup(&Value::Str(k.clone())).unwrap_or_else(|| panic!("{name}: lookup {k}"));
            assert_eq!(t.key(seg, d).unwrap(), Value::Str(k.clone()));
        }
        out.insert(name, live.keys().map(|k| k[1..].parse().unwrap()).collect());
    }
    out
}

fn model(tables: &[(&str, std::ops::Range<u64>)]) -> BTreeMap<String, BTreeSet<u64>> {
    tables.iter().map(|(n, r)| (n.to_string(), r.clone().collect())).collect()
}

/// The format of the newest committed version on disk.
fn format_on_disk(path: &Path) -> u32 {
    if path.is_dir() {
        let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
        return cur.get("format").map_or(1, |f| f.as_u64().unwrap() as u32);
    }
    meta_slots(path).into_iter().flatten().max_by_key(|m| m.txn).unwrap().format
}

/// The single file's two meta slots.
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

fn put(src: &Source, table: &str, d: Document) -> Document {
    src.put(table, &d);
    d
}

#[test]
fn transactions_span_tables_single_file() {
    transactions_span_tables("db.strata");
}

#[test]
fn transactions_span_tables_directory() {
    transactions_span_tables("db.dir");
}

fn transactions_span_tables(name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let src = Arc::new(Source::default());
    let o = options(&src, layout_of(&path));
    let db = Database::create(&path, schema("papers"), o.clone()).unwrap();
    assert_eq!(format_on_disk(&path), 1);
    db.create_table(schema("authors")).unwrap();
    assert_eq!(format_on_disk(&path), 2);
    let names: Vec<String> = db.tables().iter().map(|s| s.table.clone()).collect();
    assert_eq!(names, ["papers", "authors"]);
    assert_eq!(db.schema().table, "papers");

    // One transaction over both tables; the same key in two tables is two rows.
    let mut tx = db.begin().unwrap();
    for i in 0..20 {
        tx.insert(put(&src, "papers", doc(i, 0))).unwrap();
    }
    {
        let mut a = tx.table("AUTHORS").unwrap();
        assert_eq!(a.schema().table, "authors");
        for i in 10..40 {
            a.insert(put(&src, "authors", doc(i, 1))).unwrap();
        }
        assert!(matches!(a.insert(doc(10, 2)), Err(Error::Schema(_))), "a duplicate within the table");
        assert!(a.delete(&Value::Str(key(39))).unwrap());
        assert!(!a.delete(&Value::Str(key(5))).unwrap(), "key 5 is a paper, not an author");
        assert!(a.set(&Value::Str(key(10)), "n", Value::Int(5)).unwrap());
    }
    assert!(matches!(tx.table("venues"), Err(Error::Schema(_))));
    tx.commit().unwrap();
    let mut want = model(&[("papers", 0..20), ("authors", 10..39)]);
    assert_eq!(state(&db), want);

    // A rolled-back transaction leaves both tables alone.
    let mut tx = db.begin().unwrap();
    tx.delete(&Value::Str(key(0))).unwrap();
    tx.table("authors").unwrap().delete(&Value::Str(key(10))).unwrap();
    tx.rollback();
    assert_eq!(state(&db), want);

    // Versions are read from the row's own table, in the memtable and on disk.
    for flushed in [false, true] {
        let mut tx = db.begin().unwrap();
        assert!(!tx.table("authors").unwrap().upsert_if_newer(doc(10, 4), "n").unwrap(), "authors' k10 has n = 5 (flushed: {flushed})");
        assert!(tx.upsert_if_newer(put(&src, "papers", doc(10, 3)), "n").unwrap(), "papers' k10 has n = 0 (flushed: {flushed})");
        tx.rollback();
        db.flush().unwrap();
    }

    // The WAL holds writes to both tables; reopening replays them by table.
    let mut tx = db.begin().unwrap();
    tx.table("authors").unwrap().upsert(put(&src, "authors", doc(50, 2))).unwrap();
    tx.delete(&Value::Str(key(1))).unwrap();
    tx.table("authors").unwrap().delete(&Value::Str(key(11))).unwrap();
    tx.insert(put(&src, "papers", doc(60, 2))).unwrap();
    tx.commit().unwrap();
    want.get_mut("authors").unwrap().extend([50]);
    want.get_mut("authors").unwrap().remove(&11);
    want.get_mut("papers").unwrap().remove(&1);
    want.get_mut("papers").unwrap().insert(60);
    assert_eq!(state(&db), want);
    drop(db);
    let db = Database::open(&path, o.clone()).unwrap();
    assert_eq!(state(&db), want);
    db.check().unwrap();

    // Flushes write each table's own segments; stats name the table.
    db.flush().unwrap();
    let stats = db.file_stats();
    let tables: BTreeSet<&str> = stats.segments.iter().map(|s| s.table.as_str()).collect();
    assert_eq!(tables, BTreeSet::from(["papers", "authors"]));
    let snap = db.snapshot();
    let ids = |t: &str| -> BTreeSet<u64> { snap.table(t).unwrap().segments().iter().map(|s| s.id).collect() };
    assert!(ids("papers").is_disjoint(&ids("authors")), "segment ids are unique across tables");
    drop(snap);

    // Merges run per table: each table ends with one segment of its own rows.
    for round in 0..3u64 {
        let mut tx = db.begin().unwrap();
        for i in 0..10 {
            tx.upsert(put(&src, "papers", doc(100 + round * 10 + i, 0))).unwrap();
            tx.table("authors").unwrap().upsert(put(&src, "authors", doc(200 + round * 10 + i, 0))).unwrap();
        }
        tx.commit().unwrap();
        db.flush().unwrap();
        want.get_mut("papers").unwrap().extend(100 + round * 10..110 + round * 10);
        want.get_mut("authors").unwrap().extend(200 + round * 10..210 + round * 10);
    }
    let stats = db.merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
    assert!(stats.merges >= 2, "{stats:?}");
    let snap = db.snapshot();
    assert_eq!(snap.table("papers").unwrap().segments().len(), 1);
    assert_eq!(snap.table("authors").unwrap().segments().len(), 1);
    drop(snap);
    assert_eq!(state(&db), want);
    db.vacuum().unwrap();
    db.check().unwrap();
    drop(db);
    let db = Database::open(&path, o).unwrap();
    assert_eq!(state(&db), want);
    assert_eq!(format_on_disk(&path), 2);
    db.check().unwrap();
}

#[test]
fn create_and_drop_tables_single_file() {
    create_and_drop_tables("db.strata");
}

#[test]
fn create_and_drop_tables_directory() {
    create_and_drop_tables("db.dir");
}

fn create_and_drop_tables(name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let src = Arc::new(Source::default());
    let o = options(&src, layout_of(&path));
    let db = Database::create(&path, schema("t"), o.clone()).unwrap();
    for bad in ["T", "t", "9lives", "a-b", "", "naïve"] {
        assert!(matches!(db.create_table(schema(bad)), Err(Error::Schema(_))), "{bad:?}");
    }
    assert!(matches!(db.drop_table("t"), Err(Error::Schema(_))), "the first table stays");
    assert!(matches!(db.drop_table("nope"), Err(Error::Schema(_))));
    db.create_table(schema("u")).unwrap();
    db.create_table(schema("v_2")).unwrap();
    assert!(matches!(db.create_table(schema("U")), Err(Error::Schema(_))), "names are unique in any case");

    let mut tx = db.begin().unwrap();
    for i in 0..30 {
        tx.insert(put(&src, "t", doc(i, 0))).unwrap();
        tx.table("u").unwrap().insert(put(&src, "u", doc(i, 0))).unwrap();
        tx.table("v_2").unwrap().insert(put(&src, "v_2", doc(i, 0))).unwrap();
    }
    tx.commit().unwrap();
    db.flush().unwrap();
    // Unflushed rows of the table about to go, and of one that stays.
    let mut tx = db.begin().unwrap();
    tx.table("u").unwrap().insert(put(&src, "u", doc(99, 0))).unwrap();
    tx.table("v_2").unwrap().insert(put(&src, "v_2", doc(99, 0))).unwrap();
    tx.commit().unwrap();

    // A snapshot taken before the drop still reads the dropped table.
    let before = db.snapshot();
    db.drop_table("U").unwrap();
    assert!(db.table("u").is_none());
    assert!(db.snapshot().table("u").is_none());
    let old = before.table("u").unwrap();
    assert_eq!(live_keys(&old).len(), 31);
    assert!(old.lookup(&Value::Str(key(99))).is_some());
    drop(old);
    drop(before);
    let want = model(&[("t", 0..30), ("v_2", 0..30)]);
    let mut want_v = want.clone();
    want_v.get_mut("v_2").unwrap().insert(99);
    assert_eq!(state(&db), want_v);
    assert!(matches!(db.begin().unwrap().table("u"), Err(Error::Schema(_))));
    if path.is_dir() {
        // The dropped table's files are gone, and nothing is left unnamed.
        let segs = std::fs::read_dir(&path).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("seg-")).count();
        assert_eq!(segs, db.file_stats().segments.len());
    }
    db.check().unwrap();

    // Recreated, the table starts empty: no logged write of the old one
    // lands in it, on reopen either.
    db.create_table(schema("u")).unwrap();
    let mut tx = db.begin().unwrap();
    tx.table("u").unwrap().insert(put(&src, "u", doc(7, 1))).unwrap();
    tx.commit().unwrap();
    want_v.insert("u".into(), [7].into());
    assert_eq!(state(&db), want_v);
    drop(db);
    let db = Database::open(&path, o.clone()).unwrap();
    assert_eq!(state(&db), want_v);
    let names: Vec<String> = db.tables().iter().map(|s| s.table.clone()).collect();
    assert_eq!(names, ["t", "v_2", "u"]);

    // Back to the first table alone: format 1 again (in both meta slots,
    // which versions from before tables check), and the freed pages go
    // back to the file.
    db.drop_table("u").unwrap();
    db.drop_table("v_2").unwrap();
    assert_eq!(format_on_disk(&path), 1);
    if !path.is_dir() {
        let formats: Vec<u32> = meta_slots(&path).into_iter().map(|m| m.unwrap().format).collect();
        assert_eq!(formats, [1, 1]);
    }
    db.vacuum().unwrap();
    assert_eq!(state(&db), model(&[("t", 0..30)]));
    db.check().unwrap();
    drop(db);
    let db = Database::open(&path, o).unwrap();
    assert_eq!(state(&db), model(&[("t", 0..30)]));
    // The first table's own writes still work as before tables.
    let mut tx = db.begin().unwrap();
    tx.delete(&Value::Str(key(0))).unwrap();
    tx.commit().unwrap();
    db.flush().unwrap();
    assert_eq!(state(&db), model(&[("t", 1..30)]));
    assert_eq!(format_on_disk(&path), 1);
    db.check().unwrap();
}

#[test]
fn bulk_loads_into_a_named_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let src = Arc::new(Source::default());
    let db = Database::create(&path, schema("t"), DbOptions { segment_max_docs: 40, ..options(&src, Layout::SingleFile) }).unwrap();
    db.create_table(schema("u")).unwrap();
    db.bulk_load((0..50).map(|i| doc(i, 0))).unwrap();
    db.bulk_load_into("u", (0..100).map(|i| doc(i, 0))).unwrap();
    assert!(matches!(db.bulk_load_into("u", [doc(5, 0)]), Err(Error::Schema(_))), "key 5 is live in u");
    assert!(matches!(db.bulk_load_into("w", [doc(5, 0)]), Err(Error::Schema(_))));
    db.bulk_load_into("U", [doc(200, 0)]).unwrap();
    let mut want = model(&[("t", 0..50), ("u", 0..100)]);
    want.get_mut("u").unwrap().insert(200);
    assert_eq!(state(&db), want);
    let snap = db.snapshot();
    assert_eq!(snap.table("u").unwrap().segments().len(), 4);
    drop(snap);
    db.check().unwrap();
}

/// Files written by Strata before tables (main at 6b85211, with the
/// program in `tests/fixtures/format1/README.md`): a single file and a
/// directory, each with two flushes, deletes, values set in place and a
/// transaction left in the WAL. They open, take writes and stay format 1.
#[test]
fn files_from_before_tables_open_and_stay_format_1() {
    for name in ["one.strata", "one.dir"] {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format1");
        let path = dir.path().join(name);
        copy(&fixtures.join(name), &path);
        if !path.is_dir() {
            copy(&fixtures.join(format!("{name}-wal")), &dir.path().join(format!("{name}-wal")));
        }
        let src = Arc::new(Source::default());
        let o = options(&src, layout_of(&path));
        let db = Database::open(&path, o.clone()).unwrap();
        // 0..40 less the deletes of 0..5 and of 10.
        let mut want: BTreeSet<u64> = (5..40).collect();
        want.remove(&10);
        assert_eq!(state(&db), BTreeMap::from([("t".to_string(), want.clone())]), "{name}");
        // n was set to 7 on 5..8 and k11 was upserted with n = 2.
        let mut tx = db.begin().unwrap();
        for (i, n) in [(5, 7), (7, 7), (8, 0), (11, 2), (35, 1)] {
            assert!(!tx.upsert_if_newer(doc(i, n), "n").unwrap(), "{name}: k{i} has n = {n}");
            assert!(tx.upsert_if_newer(put(&src, "t", doc(i, n + 1)), "n").unwrap(), "{name}: k{i} has n = {n}");
        }
        tx.insert(put(&src, "t", doc(100, 0))).unwrap();
        tx.commit().unwrap();
        db.flush().unwrap();
        for i in want.iter().copied().filter(|i| ![5, 7, 8, 11, 35].contains(i)).collect::<Vec<_>>() {
            src.put("t", &doc(i, 0));
        }
        db.merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
        db.check().unwrap();
        want.insert(100);
        assert_eq!(state(&db), BTreeMap::from([("t".to_string(), want.clone())]));
        assert_eq!(format_on_disk(&path), 1, "{name}");
        if path.is_dir() {
            let cur = std::fs::read_to_string(path.join("CURRENT")).unwrap();
            assert!(!cur.contains("format"), "{cur}");
        }
        drop(db);
        let db = Database::open(&path, o).unwrap();
        assert_eq!(state(&db), BTreeMap::from([("t".to_string(), want)]));
    }
}

fn copy(from: &Path, to: &Path) {
    if from.is_dir() {
        std::fs::create_dir(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    } else {
        std::fs::copy(from, to).unwrap();
    }
}

/// Random transactions over three tables, CREATE and DROP of two of them,
/// flushes, merges, vacuums, bulk loads and reopens against a model.
#[test]
fn random_operations_across_tables_match_model() {
    for seed in 0..4 {
        random_operations(seed, if seed % 2 == 0 { "db.strata" } else { "db.dir" });
    }
}

fn random_operations(seed: u64, name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let src = Arc::new(Source::default());
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let o = DbOptions { memtable_max_docs: 150, memtable_chunk_docs: 64, segment_max_docs: 120, ..options(&src, layout_of(&path)) };
    let policy = MergePolicy { merge_factor: 3, floor_docs: 40, max_deleted_ratio: 0.4, ..MergePolicy::default() };
    let mut db = Some(Database::create(&path, schema("t"), o.clone()).unwrap());
    let mut model: BTreeMap<String, BTreeSet<u64>> = BTreeMap::from([("t".to_string(), BTreeSet::new())]);
    let mut version = 0i64;
    let mut fresh = 10_000u64;
    for step in 0..400 {
        let d = db.as_ref().unwrap();
        let names: Vec<String> = model.keys().cloned().collect();
        match rng.gen_range(0..100) {
            0..=64 => {
                let commit = rng.gen_bool(0.85);
                let mut local = model.clone();
                let mut tx = d.begin().unwrap();
                for _ in 0..rng.gen_range(1..40) {
                    let t = &names[rng.gen_range(0..names.len())];
                    let i = rng.gen_range(0..300u64);
                    version += 1;
                    let live = local.get_mut(t).unwrap();
                    let mut w = tx.table(t).unwrap();
                    match rng.gen_range(0..4) {
                        0 => {
                            let r = w.insert(doc(i, version));
                            assert_eq!(r.is_ok(), !live.contains(&i));
                            if r.is_ok() {
                                src.put(t, &doc(i, version));
                            }
                            live.insert(i);
                        }
                        1 => {
                            w.upsert(put(&src, t, doc(i, version))).unwrap();
                            live.insert(i);
                        }
                        2 => assert_eq!(w.set(&Value::Str(key(i)), "n", Value::Int(version)).unwrap(), live.contains(&i)),
                        _ => assert_eq!(w.delete(&Value::Str(key(i))).unwrap(), live.remove(&i)),
                    }
                }
                if commit {
                    tx.commit().unwrap();
                    model = local;
                } else {
                    tx.rollback();
                }
            }
            65..=71 => d.flush().unwrap(),
            72..=78 => {
                d.merge(&policy).unwrap();
            }
            79..=80 => {
                d.vacuum().unwrap();
            }
            81..=84 => {
                let t = &names[rng.gen_range(0..names.len())];
                let ids = fresh..fresh + rng.gen_range(1..20);
                fresh += 100;
                d.bulk_load_into(t, ids.clone().map(|i| put(&src, t, doc(i, 0)))).unwrap();
                model.get_mut(t).unwrap().extend(ids);
            }
            85..=90 => {
                let t = ["u", "v"][rng.gen_range(0..2)];
                if model.contains_key(t) {
                    d.drop_table(t).unwrap();
                    model.remove(t);
                } else {
                    d.create_table(schema(t)).unwrap();
                    model.insert(t.to_string(), BTreeSet::new());
                }
            }
            _ => {
                // Close before reopening: the file lock admits one handle.
                drop(db.take());
                db = Some(Database::open(&path, o.clone()).unwrap());
            }
        }
        assert_eq!(state(db.as_ref().unwrap()), model, "seed {seed} step {step}");
        let format = if model.len() == 1 { 1 } else { 2 };
        // A version on disk follows every commit except a WAL append.
        if step % 25 == 0 {
            db.as_ref().unwrap().check().unwrap();
            db.as_ref().unwrap().flush().unwrap();
            assert_eq!(format_on_disk(&path), format, "seed {seed} step {step}");
        }
    }
    db.as_ref().unwrap().check().unwrap();
}

const CHILD_ENV: &str = "STRATA_TABLES_CRASH_CHILD";

/// Tables as the crash log names them: one letter each.
type Tables = BTreeMap<char, BTreeSet<u64>>;

fn recovered(db: &Database) -> Tables {
    state(db).into_iter().map(|(name, keys)| (name.chars().next().unwrap(), keys)).collect()
}

/// The child: runs until killed. Not a real test unless `CHILD_ENV` is set.
/// Logs `B n ops` before a commit and `C n` after it returns; an operation
/// is a table letter, then `i`, `u` or `d` and a key number, or `+0` (the
/// table was created) or `-0` (dropped).
#[test]
fn tables_crash_child() {
    let Ok(spec) = std::env::var(CHILD_ENV) else { return };
    let parts: Vec<&str> = spec.split('|').collect();
    let (db_path, log_path) = (Path::new(parts[0]), Path::new(parts[1]));
    let seed: u64 = parts[2].parse().unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let src = Arc::new(Source::default());
    let o = DbOptions {
        sync: seed.is_multiple_of(2),
        memtable_max_docs: rng.gen_range(20..300),
        memtable_chunk_docs: rng.gen_range(1..200),
        segment_max_docs: rng.gen_range(50..400),
        ..options(&src, layout_of(db_path))
    };
    let db = if db_path.exists() { Database::open(db_path, o).unwrap() } else { Database::create(db_path, schema("t"), o).unwrap() };
    let mut live = recovered(&db);
    for (&t, keys) in &live {
        for &i in keys {
            src.put(&t.to_string(), &doc(i, 0));
        }
    }
    let mut log = OpenOptions::new().create(true).append(true).open(log_path).unwrap();
    let policy = MergePolicy { merge_factor: 3, floor_docs: 50, max_deleted_ratio: 0.3, ..MergePolicy::default() };
    let mut fresh = 1_000_000 * (seed + 1);
    let mut n = 0u64;
    let ddl = |n: &mut u64, log: &mut File, live: &mut Tables, t: char| {
        *n += 1;
        let create = !live.contains_key(&t);
        note(log, format!("B {n} {t}{}0\n", if create { '+' } else { '-' }));
        if create {
            db.create_table(schema(&t.to_string())).unwrap();
            live.insert(t, BTreeSet::new());
        } else {
            db.drop_table(&t.to_string()).unwrap();
            live.remove(&t);
        }
        note(log, format!("C {n}\n"));
    };
    if !live.contains_key(&'u') {
        ddl(&mut n, &mut log, &mut live, 'u');
    }
    loop {
        let tables: Vec<char> = live.keys().copied().collect();
        match rng.gen_range(0..100) {
            0..=71 => {
                n += 1;
                let mut ops = Vec::new();
                let mut tx = db.begin().unwrap();
                for _ in 0..rng.gen_range(1..30) {
                    let t = tables[rng.gen_range(0..tables.len())];
                    let name = t.to_string();
                    let keys = live.get_mut(&t).unwrap();
                    let i = rng.gen_range(0..1500u64);
                    let d = doc(i, n as i64);
                    let mut w = tx.table(&name).unwrap();
                    if keys.contains(&i) && rng.gen_bool(0.5) {
                        assert!(w.delete(&Value::Str(key(i))).unwrap());
                        keys.remove(&i);
                        ops.push(format!("{t}d{i}"));
                    } else if keys.contains(&i) || rng.gen_bool(0.5) {
                        w.upsert(put(&src, &name, d)).unwrap();
                        keys.insert(i);
                        ops.push(format!("{t}u{i}"));
                    } else {
                        w.insert(put(&src, &name, d)).unwrap();
                        keys.insert(i);
                        ops.push(format!("{t}i{i}"));
                    }
                }
                note(&mut log, format!("B {n} {}\n", ops.join(",")));
                tx.commit().unwrap();
                note(&mut log, format!("C {n}\n"));
            }
            72..=80 => db.flush().unwrap(),
            81..=87 => {
                db.merge(&policy).unwrap();
            }
            88..=89 => {
                db.vacuum().unwrap();
            }
            90..=94 => {
                n += 1;
                let t = tables[rng.gen_range(0..tables.len())];
                let ids: Vec<u64> = (fresh..fresh + rng.gen_range(1..30)).collect();
                fresh += 1000;
                let docs: Vec<_> = ids.iter().map(|&i| put(&src, &t.to_string(), doc(i, 0))).collect();
                let ops: Vec<String> = ids.iter().map(|i| format!("{t}i{i}")).collect();
                note(&mut log, format!("B {n} {}\n", ops.join(",")));
                db.bulk_load_into(&t.to_string(), docs).unwrap();
                live.get_mut(&t).unwrap().extend(ids);
                note(&mut log, format!("C {n}\n"));
            }
            _ => ddl(&mut n, &mut log, &mut live, 'v'),
        }
    }
}

/// Appends a line with one `write` (a kill cannot split it).
fn note(log: &mut File, line: String) {
    log.write_all(line.as_bytes()).unwrap();
}

/// A parsed log line: (is a `B` line, transaction number, operations as
/// (table, op, key)).
type LogLine = (bool, u64, Vec<(char, char, u64)>);

/// `None` for a line torn by the kill.
fn parse_line(line: &str) -> Option<LogLine> {
    let mut it = line.splitn(3, ' ');
    let (tag, n) = (it.next()?, it.next()?.parse().ok()?);
    match (tag, it.next()) {
        ("B", Some(ops)) => {
            let ops = ops
                .split(',')
                .map(|o| {
                    let mut c = o.chars();
                    Some((c.next()?, c.next()?, c.as_str().parse().ok()?))
                })
                .collect::<Option<_>>()?;
            Some((true, n, ops))
        }
        ("C", None) => Some((false, n, Vec::new())),
        _ => None,
    }
}

/// States the database may recover to: after each transaction from the
/// last acknowledged one to the last attempted one.
fn candidates(start: &Tables, log: &str) -> Vec<Tables> {
    let mut acked = 0u64;
    let mut txns = Vec::new();
    for (begin, n, ops) in log.lines().filter_map(parse_line) {
        if begin {
            txns.push((n, ops));
        } else {
            acked = n;
        }
    }
    let mut state = start.clone();
    let mut out = Vec::new();
    if txns.first().is_none_or(|(n, _)| *n > acked) {
        out.push(state.clone());
    }
    for (n, ops) in txns {
        for (t, op, i) in ops {
            match op {
                '+' => {
                    state.insert(t, BTreeSet::new());
                }
                '-' => {
                    state.remove(&t);
                }
                'd' => {
                    state.get_mut(&t).unwrap().remove(&i);
                }
                _ => {
                    state.get_mut(&t).unwrap().insert(i);
                }
            }
        }
        if n >= acked {
            out.push(state.clone());
        }
    }
    out
}

#[test]
fn killed_writer_recovers_a_committed_prefix_of_every_table() {
    killed_writer_recovers("db.strata");
}

#[test]
fn killed_writer_recovers_a_committed_prefix_of_every_table_in_directory_layout() {
    killed_writer_recovers("db.dir");
}

fn killed_writer_recovers(name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join(name);
    let exe = std::env::current_exe().unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let mut state: Tables = BTreeMap::from([('t', BTreeSet::new())]);
    let src = Arc::new(Source::default());
    let iterations = std::env::var("STRATA_CRASH_ITERS").map_or(20, |v| v.parse().unwrap());
    for iter in 0..iterations {
        let log_path = dir.path().join(format!("log{iter}"));
        let spec = format!("{}|{}|{iter}", db_path.display(), log_path.display());
        let mut child = Command::new(&exe)
            .args(["tables_crash_child", "--exact", "--test-threads=1"])
            .env(CHILD_ENV, spec)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(rng.gen_range(40..400)));
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited on its own: {status}");
        }
        child.kill().unwrap();
        child.wait().unwrap();

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let cands = candidates(&state, &log);
        let db = Database::open(&db_path, options(&src, layout_of(&db_path))).unwrap();
        let got = recovered(&db);
        let txns = log.lines().filter(|l| l.starts_with("C ")).count();
        assert!(
            cands.contains(&got),
            "iteration {iter}: recovered {:?} keys per table, not a committed prefix ({} candidates)",
            got.iter().map(|(t, k)| (*t, k.len())).collect::<Vec<_>>(),
            cands.len()
        );
        db.check().unwrap();
        eprintln!("iteration {iter}: {txns} commits acknowledged, recovered {} tables", got.len());
        state = got;
    }
}

#[test]
fn candidate_states_from_log() {
    let start: Tables = BTreeMap::from([('t', [1, 2].into())]);
    let log = "B 1 ti3,td1\nC 1\nB 2 u+0\nC 2\nB 3 ui9,td2\n";
    let c = candidates(&start, log);
    assert_eq!(c, vec![BTreeMap::from([('t', [2, 3].into()), ('u', BTreeSet::new())]), BTreeMap::from([('t', [3].into()), ('u', [9].into())])]);
    assert_eq!(candidates(&start, "B 1 t-0\n"), vec![start.clone(), BTreeMap::new()]);
}
