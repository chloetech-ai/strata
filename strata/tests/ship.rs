//! Shipping numbered transactions from a primary to a replica. A database
//! that keeps a ship log (`DbOptions::ship_log_bytes`) keeps its most
//! recent transactions as it logged them (`Database::log_from`), and a
//! replica applies them under the primary's terms and numbers
//! (`Database::apply_shipped`), so that both hold the same rows, each with
//! the same number. A replica refuses transactions that do not follow its
//! last one, come from an older term, are damaged, or do not apply to its
//! rows.

mod storage_util;

use std::sync::{Arc, Mutex};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use storage_util::*;
use strata::db::Db;
use strata::query::Params;
use strata::storage::{Applied, CheckpointOptions, DbOptions, Layout, LogRead, MergePolicy, SeqNo, Shipped};
use strata::{Document, Field, FieldType, Schema, Value};

fn options(layout: Layout) -> DbOptions {
    // Commits never fill the memtable: only the test flushes.
    DbOptions { memtable_max_docs: 1_000_000, memtable_chunk_docs: 64, segment_max_docs: 100, sync: false, layout, ship_log_bytes: 1 << 30, ..DbOptions::default() }
}

fn path_for(dir: &std::path::Path, name: &str, layout: Layout) -> std::path::PathBuf {
    dir.join(if layout == Layout::Directory { format!("{name}.dir") } else { format!("{name}.strata") })
}

/// A second table, so that transactions write several.
fn other() -> Schema {
    Schema::new("u", vec![Field::new("id", FieldType::Int), Field::new("x", FieldType::Float)], Some("id")).unwrap()
}

fn txns(r: LogRead) -> Vec<Shipped> {
    match r {
        LogRead::Txns(t) => t,
        LogRead::Gone { oldest } => panic!("the log holds numbers from {oldest} on only"),
    }
}

fn last(db: &Db) -> SeqNo {
    db.storage().numbering().unwrap().last()
}

/// The transaction before `seq + 1` as `db` has it.
fn prev(db: &Db, seq: u64) -> SeqNo {
    SeqNo { term: db.storage().numbering().unwrap().term_at(seq), seq }
}

/// Ships what `replica` lacks of `primary`'s log, at most `max` bytes at a
/// time; returns how many transactions it shipped.
fn ship(primary: &Db, replica: &Db, max: usize) -> usize {
    let mut n = 0;
    loop {
        let at = last(replica);
        let got = txns(primary.storage().log_from(at.seq + 1, max));
        let Some(end) = got.last().map(|t| t.seq) else { return n };
        let bytes: Vec<&[u8]> = got.iter().map(|t| &*t.bytes).collect();
        assert_eq!(replica.storage().apply_shipped(prev(primary, at.seq), &bytes).unwrap(), Applied::At(end));
        assert_eq!(last(replica), end, "the replica's last number is the last applied");
        n += got.len();
    }
}

/// Every document of the first table, in key order, and the second
/// table's rows.
fn documents(db: &Db) -> (Vec<Document>, Vec<Vec<Value>>) {
    let mut docs: Vec<Document> = db.documents(&db.snapshot()).map(|d| d.unwrap()).collect();
    docs.sort_by_key(|d| d[0].as_str().unwrap().to_string());
    let mut u = db.query("SELECT id, x FROM u LIMIT 100000", &Params::new()).unwrap().rows;
    u.sort_by_key(|r| r[0].as_i64());
    (docs, u)
}

/// Each row's number, by table and key.
fn numbers(db: &Db) -> Vec<Vec<(Value, i64)>> {
    ["t", "u"]
        .iter()
        .filter(|t| db.storage().table(t).is_some())
        .map(|t| {
            let r = db.query(&format!("SELECT id, seq_no() FROM {t} LIMIT 100000"), &Params::new()).unwrap();
            let mut rows: Vec<(Value, i64)> = r.rows.iter().map(|r| (r[0].clone(), r[1].as_i64().unwrap())).collect();
            rows.sort_by(|a, b| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)));
            rows
        })
        .collect()
}

/// The replica holds the primary's rows with the primary's numbers, at
/// the primary's last number, under the same terms.
fn same(primary: &Db, replica: &Db, what: &str) {
    assert_eq!(documents(replica), documents(primary), "{what}: rows");
    assert_eq!(numbers(replica), numbers(primary), "{what}: numbers");
    let (p, r) = (primary.storage().numbering().unwrap(), replica.storage().numbering().unwrap());
    assert_eq!(r.last(), p.last(), "{what}: last number");
    for s in 0..=p.seq {
        assert_eq!(r.term_at(s), p.term_at(s), "{what}: term of {s}");
    }
}

/// A random transaction over both tables: upserts, inserts, deletes,
/// values set in place and added to.
fn write(db: &Db, rng: &mut StdRng, keys: u64) -> Option<u64> {
    let mut tx = db.storage().begin().unwrap();
    for _ in 0..rng.gen_range(1..6) {
        let i = rng.gen_range(0..keys);
        let (k, v) = (Value::Str(key(i)), rng.gen_range(0..1000));
        match rng.gen_range(0..8) {
            0 | 1 => tx.upsert(doc(i, v)).unwrap(),
            2 => {
                let _ = tx.insert(doc(i, v));
            }
            3 => {
                tx.delete(&k).unwrap();
            }
            4 => {
                tx.set(&k, "n", Value::Int(v)).unwrap();
            }
            5 => {
                tx.add(&k, "n", &Value::Int(1)).unwrap();
            }
            6 => tx.table("u").unwrap().upsert(vec![Value::Int(i as i64), Value::Float(v as f64 / 7.0)]).unwrap(),
            _ => {
                tx.table("u").unwrap().delete(&Value::Int(i as i64)).unwrap();
            }
        }
    }
    tx.commit_seq().unwrap()
}

/// A replica that applies what its primary ships holds the same rows with
/// the same numbers as the primary, whatever each does on its own in
/// between (flushes, background flushes, merges, reopening, checkpoints)
/// and in either layout, across a new term of the primary's; rows from
/// before numbering, the same on both, keep number 0.
#[test]
fn a_replica_holds_what_its_primary_wrote() {
    for (seed, pl, rl) in [(1, Layout::SingleFile, Layout::Directory), (2, Layout::Directory, Layout::SingleFile), (3, Layout::SingleFile, Layout::SingleFile)] {
        let dir = tempfile::tempdir().unwrap();
        let (pp, rp) = (path_for(dir.path(), "primary", pl), path_for(dir.path(), "replica", rl));
        let (po, ro) = (DbOptions { memtable_chunk_docs: 8, ..options(pl) }, DbOptions { segment_max_docs: 40, ..options(rl) });
        let mut primary = Db::create(&pp, schema(), po.clone()).unwrap();
        let mut replica = Db::create(&rp, schema(), ro.clone()).unwrap();
        for db in [&primary, &replica] {
            db.create_table(other()).unwrap();
            // The same rows before numbering, on both: number 0.
            let mut tx = db.storage().begin().unwrap();
            for i in 0..30 {
                tx.upsert(doc(i, 3)).unwrap();
            }
            tx.commit().unwrap();
            db.storage().start_seq_no().unwrap();
        }
        same(&primary, &replica, "start");
        let mut rng = StdRng::seed_from_u64(seed);
        let copies = dir.path().join("copies");
        std::fs::create_dir(&copies).unwrap();
        let mut shipped = 0;
        for round in 0..30 {
            for _ in 0..rng.gen_range(1..12) {
                write(&primary, &mut rng, 200);
            }
            let what = format!("seed {seed}, round {round}");
            // One transaction at a time, a few, or all at once.
            shipped += ship(&primary, &replica, [1, 2_000, usize::MAX][rng.gen_range(0..3)]);
            same(&primary, &replica, &what);
            let (db, path, opts) = match rng.gen_bool(0.5) {
                true => (&mut primary, &pp, &po),
                false => (&mut replica, &rp, &ro),
            };
            match rng.gen_range(0..7) {
                0 => db.storage().flush().unwrap(),
                1 => db.storage().background_flush().unwrap(),
                2 => {
                    db.storage().flush().unwrap();
                    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
                }
                3 => {
                    drop(std::mem::replace(db, Db::create(&dir.path().join(format!("x{round}.strata")), schema(), DbOptions::default()).unwrap()));
                    *db = Db::open(path, opts.clone()).unwrap();
                }
                4 => {
                    let to = path_for(&copies, &format!("c{round}"), Layout::SingleFile);
                    db.storage().checkpoint(&to, &CheckpointOptions::default()).unwrap();
                    let copy = Db::open(&to, opts.clone()).unwrap();
                    assert_eq!(numbers(&copy), numbers(db), "{what}: checkpoint");
                }
                5 if std::ptr::eq(db, &primary) => {
                    // A new term: the replica follows with the next write.
                    let term = primary.storage().numbering().unwrap().term + 1;
                    primary.storage().begin_term(term).unwrap();
                    write(&primary, &mut rng, 200);
                }
                _ => {}
            }
            shipped += ship(&primary, &replica, usize::MAX);
            same(&primary, &replica, &format!("{what}, after"));
        }
        let terms = primary.storage().numbering().unwrap().term;
        assert!(terms > 1 && shipped > 100, "seed {seed}: {terms} terms, {shipped} shipped");
        replica.storage().check().unwrap();
    }
}

/// A replica applies nothing that does not follow its last transaction,
/// and refuses a batch with a damaged transaction, one that skips a
/// number, or one from an older term than its own, applying none of the
/// batch; a transaction whose operations do not apply to its rows is not
/// applied, and those before it are.
#[test]
fn a_replica_applies_only_what_follows() {
    let dir = tempfile::tempdir().unwrap();
    let opts = options(Layout::SingleFile);
    let primary = Db::create(&dir.path().join("p.strata"), schema(), opts.clone()).unwrap();
    let replica = Db::create(&dir.path().join("r.strata"), schema(), opts.clone()).unwrap();
    assert!(replica.storage().apply_shipped(SeqNo::default(), &[]).unwrap_err().to_string().contains("does not number its writes"));
    for db in [&primary, &replica] {
        db.storage().start_seq_no().unwrap();
    }
    for i in 0..6 {
        let mut tx = primary.storage().begin().unwrap();
        tx.upsert(doc(i, i as i64)).unwrap();
        tx.commit().unwrap();
    }
    let log = txns(primary.storage().log_from(1, usize::MAX));
    assert_eq!(log.iter().map(|t| t.seq.seq).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6]);
    let bytes = |r: std::ops::Range<usize>| log[r].iter().map(|t| &*t.bytes).collect::<Vec<&[u8]>>();
    assert_eq!(replica.storage().apply_shipped(prev(&primary, 0), &bytes(0..3)).unwrap(), Applied::At(SeqNo { term: 1, seq: 3 }));
    // Not after its last: nothing, and where it stands.
    for p in [SeqNo { term: 1, seq: 2 }, SeqNo { term: 1, seq: 4 }, SeqNo { term: 2, seq: 3 }] {
        assert_eq!(replica.storage().apply_shipped(p, &bytes(3..4)).unwrap(), Applied::Mismatch { seq: 3, term: 1 }, "{p:?}");
    }
    // A batch that skips a number, or holds a damaged transaction.
    let skips = vec![&*log[3].bytes, &*log[5].bytes];
    assert!(replica.storage().apply_shipped(prev(&primary, 3), &skips).unwrap_err().to_string().contains("does not follow 4"));
    let mut damaged = log[4].bytes.to_vec();
    let n = damaged.len();
    damaged[n / 2] ^= 1;
    assert!(replica.storage().apply_shipped(prev(&primary, 3), &[&*log[3].bytes, &damaged]).unwrap_err().to_string().contains("damaged"));
    assert_eq!(last(&replica).seq, 3, "nothing of a refused batch");
    assert!(replica.storage().apply_shipped(prev(&primary, 3), &[&log[3].bytes[..20]]).is_err());
    assert_eq!(replica.storage().apply_shipped(prev(&primary, 3), &bytes(3..6)).unwrap(), Applied::At(SeqNo { term: 1, seq: 6 }));

    // An old primary: a copy that went on under term 1 after the primary
    // began term 2.
    let old_path = dir.path().join("old.strata");
    primary.storage().checkpoint(&old_path, &CheckpointOptions::default()).unwrap();
    let old = Db::open(&old_path, opts.clone()).unwrap();
    primary.storage().begin_term(2).unwrap();
    upsert_one(&primary, 7);
    assert_eq!(ship(&primary, &replica, usize::MAX), 1);
    assert_eq!(last(&replica), SeqNo { term: 2, seq: 7 });
    assert_eq!(replica.storage().numbering().unwrap().terms, [(1, 1), (2, 7)]);
    upsert_one(&old, 8);
    let stale = txns(old.storage().log_from(7, usize::MAX));
    // Its history differs at 7 ...
    assert_eq!(replica.storage().apply_shipped(prev(&old, 6), &[&*stale[0].bytes]).unwrap(), Applied::Mismatch { seq: 7, term: 2 });
    assert_ne!(prev(&old, 7), last(&replica));
    // ... and its transactions come from an older term.
    upsert_one(&old, 9);
    let stale = txns(old.storage().log_from(8, usize::MAX));
    let e = replica.storage().apply_shipped(last(&replica), &[&*stale[0].bytes]).unwrap_err().to_string();
    assert!(e.contains("older than term 2"), "{e}");
    assert_eq!(last(&replica), SeqNo { term: 2, seq: 7 });

    // Replicas whose rows are not the primary's: a transaction that does
    // not apply is not applied, and those before it are.
    let p2 = Db::create(&dir.path().join("p2.strata"), schema(), opts.clone()).unwrap();
    upsert_one(&p2, 0);
    p2.storage().start_seq_no().unwrap();
    upsert_one(&p2, 1);
    let mut tx = p2.storage().begin().unwrap();
    tx.set(&Value::Str(key(0)), "n", Value::Int(5)).unwrap();
    tx.commit().unwrap();
    upsert_one(&p2, 2);
    let log = txns(p2.storage().log_from(1, usize::MAX));
    let log: Vec<&[u8]> = log.iter().map(|t| &*t.bytes).collect();
    // One that lacks row 0, which the primary had before numbering.
    let lacks = Db::create(&dir.path().join("lacks.strata"), schema(), opts.clone()).unwrap();
    lacks.storage().start_seq_no().unwrap();
    let e = lacks.storage().apply_shipped(SeqNo::default(), &log).unwrap_err().to_string();
    assert!(e.contains("which no row here has") && e.contains("not a copy of its primary"), "{e}");
    assert_eq!(last(&lacks).seq, 1);
    assert_eq!(numbers(&lacks)[0], [(Value::Str(key(1)), 1)]);
    // One that has a row the primary inserts.
    let has = Db::create(&dir.path().join("has.strata"), schema(), opts.clone()).unwrap();
    upsert_one(&has, 0);
    upsert_one(&has, 1);
    has.storage().start_seq_no().unwrap();
    let e = has.storage().apply_shipped(SeqNo::default(), &log).unwrap_err().to_string();
    assert!(e.contains("inserts a row it cannot"), "{e}");
    assert_eq!(last(&has).seq, 0);
    // A delete of a row a replica lacks.
    let p3 = Db::create(&dir.path().join("p3.strata"), schema(), opts.clone()).unwrap();
    upsert_one(&p3, 0);
    p3.storage().start_seq_no().unwrap();
    let mut tx = p3.storage().begin().unwrap();
    tx.delete(&Value::Str(key(0))).unwrap();
    tx.commit().unwrap();
    let log = txns(p3.storage().log_from(1, usize::MAX));
    let empty = Db::create(&dir.path().join("empty.strata"), schema(), opts.clone()).unwrap();
    empty.storage().start_seq_no().unwrap();
    let e = empty.storage().apply_shipped(SeqNo::default(), &[&*log[0].bytes]).unwrap_err().to_string();
    assert!(e.contains("deletes") && e.contains("which no row here has"), "{e}");
    assert_eq!(last(&empty).seq, 0);
}

fn upsert_one(db: &Db, i: u64) {
    let mut tx = db.storage().begin().unwrap();
    tx.upsert(doc(i, 1)).unwrap();
    tx.commit().unwrap();
}

/// The ship log keeps the newest transactions up to its bytes, exactly as
/// they were logged; opening refills it from the WAL, and once a flush has
/// emptied the WAL it holds the transactions after it. A database that
/// keeps no log has none.
#[test]
fn the_log_keeps_the_newest_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let opts = options(Layout::SingleFile);
    let db = Db::create(&path, schema(), DbOptions { ship_log_bytes: 2_000, ..opts.clone() }).unwrap();
    db.storage().start_seq_no().unwrap();
    for i in 0..50 {
        upsert_one(&db, i);
    }
    let LogRead::Gone { oldest } = db.storage().log_from(1, usize::MAX) else { panic!("2,000 bytes hold 50 transactions") };
    assert!(oldest > 1 && oldest < 50, "{oldest}");
    let kept = txns(db.storage().log_from(oldest, usize::MAX));
    assert_eq!(kept.iter().map(|t| t.seq.seq).collect::<Vec<_>>(), (oldest..=50).collect::<Vec<_>>());
    assert!(kept.iter().map(|t| t.bytes.len()).sum::<usize>() <= 2_000);
    assert_eq!(txns(db.storage().log_from(51, usize::MAX)), []);
    drop(db);

    // Opening refills it with the same bytes.
    let db = Db::open(&path, opts.clone()).unwrap();
    let all = txns(db.storage().log_from(1, usize::MAX));
    assert_eq!(all.len(), 50);
    assert_eq!(all[oldest as usize - 1..], kept[..]);
    // After a flush, from the next transaction on.
    db.storage().flush().unwrap();
    drop(db);
    let db = Db::open(&path, opts.clone()).unwrap();
    assert_eq!(db.storage().log_from(50, usize::MAX), LogRead::Gone { oldest: 51 });
    upsert_one(&db, 50);
    assert_eq!(txns(db.storage().log_from(51, usize::MAX)).len(), 1);
    assert_eq!(db.storage().log_from(50, usize::MAX), LogRead::Gone { oldest: 51 });
    drop(db);

    // A database that keeps no log.
    let none = Db::open(&path, DbOptions { ship_log_bytes: 0, ..opts }).unwrap();
    upsert_one(&none, 51);
    assert_eq!(none.storage().log_from(53, usize::MAX), LogRead::Txns(Vec::new()));
    assert_eq!(none.storage().log_from(52, usize::MAX), LogRead::Gone { oldest: 53 });
}

/// The commit hook hears the term and number of each numbered transaction
/// once it is committed, also one that fills the memtable and flushes, and
/// on a replica each one it applies; a transaction that writes nothing has
/// no number and is not heard.
#[test]
fn the_commit_hook_hears_every_number() {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { memtable_max_docs: 3, ..options(Layout::SingleFile) };
    let primary = Db::create(&dir.path().join("p.strata"), schema(), opts.clone()).unwrap();
    let replica = Db::create(&dir.path().join("r.strata"), schema(), opts).unwrap();
    let heard: Arc<Mutex<Vec<(&str, SeqNo)>>> = Arc::default();
    for (db, name) in [(&primary, "primary"), (&replica, "replica")] {
        db.storage().start_seq_no().unwrap();
        let h = heard.clone();
        db.storage().on_commit(move |s| h.lock().unwrap().push((name, s)));
    }
    for i in 0..5 {
        upsert_one(&primary, i);
    }
    primary.storage().begin().unwrap().commit().unwrap();
    let numbers = |who: &str| heard.lock().unwrap().iter().filter(|(w, _)| *w == who).map(|(_, s)| s.seq).collect::<Vec<_>>();
    assert_eq!(numbers("primary"), [1, 2, 3, 4, 5]);
    assert_eq!(primary.storage().file_stats().segments.len(), 1, "a commit flushed");
    ship(&primary, &replica, usize::MAX);
    assert_eq!(numbers("replica"), [1, 2, 3, 4, 5]);
}
