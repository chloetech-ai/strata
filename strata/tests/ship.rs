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
use strata::storage::{Applied, CheckpointOptions, DbOptions, DropOptions, Layout, LogRead, MergePolicy, SeqNo, Shipped};
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
        assert_eq!(replica.snapshot().seq_no(), Some(end.seq), "and its snapshots hold it");
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
    // Asking its last needs no writer, which a flush or a merge may hold;
    // a batch waits for it.
    {
        let _held = replica.storage().begin().unwrap();
        assert_eq!(replica.storage().apply_shipped(prev(&primary, 6), &[]).unwrap(), Applied::At(SeqNo { term: 1, seq: 6 }));
        assert_eq!(replica.storage().apply_shipped(prev(&primary, 5), &[]).unwrap(), Applied::Mismatch { seq: 6, term: 1 });
        assert!(matches!(replica.storage().apply_shipped(prev(&primary, 6), &bytes(5..6)), Err(strata::Error::Busy(_))));
    }

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

/// A replica that began a term and wrote nothing in it (it was elected,
/// or installed a copy of a primary that had begun one) takes what a
/// newer primary holds of an older term past its last: those terms hold
/// nothing, and are dropped for the transactions shipped, also once it is
/// opened again. (The cluster simulator found a shard whose replicas
/// refused them for good.)
#[test]
fn a_replica_drops_the_terms_it_wrote_nothing_in_for_its_primary_s() {
    let dir = tempfile::tempdir().unwrap();
    for (layout, sync) in [(Layout::SingleFile, false), (Layout::Directory, true)] {
        let opts = DbOptions { sync, ..options(layout) };
        let primary = Db::create(&path_for(dir.path(), "p", layout), schema(), opts.clone()).unwrap();
        let replica_path = path_for(dir.path(), "r", layout);
        let replica = Db::create(&replica_path, schema(), opts.clone()).unwrap();
        for db in [&primary, &replica] {
            db.storage().start_seq_no().unwrap();
        }
        for i in 0..3 {
            upsert_one(&primary, i);
        }
        assert_eq!(ship(&primary, &replica, usize::MAX), 3);
        // Two more of term 1 the replica did not get, and it began terms 2
        // and 3 with nothing in them; the primary is now of term 4.
        upsert_one(&primary, 3);
        upsert_one(&primary, 4);
        replica.storage().begin_term(2).unwrap();
        replica.storage().begin_term(3).unwrap();
        primary.storage().begin_term(4).unwrap();
        upsert_one(&primary, 5);
        assert_eq!(ship(&primary, &replica, usize::MAX), 3);
        assert_eq!(last(&replica), SeqNo { term: 4, seq: 6 });
        let docs = |db: &Db| db.documents(&db.snapshot()).collect::<Result<Vec<_>, _>>().unwrap().len();
        assert_eq!(replica.storage().numbering().unwrap(), primary.storage().numbering().unwrap());
        assert_eq!(primary.storage().numbering().unwrap().terms, [(1, 1), (4, 6)]);
        assert_eq!(docs(&replica), 6);
        drop(replica);
        let replica = Db::open(&replica_path, opts.clone()).unwrap();
        assert_eq!(replica.storage().numbering().unwrap(), primary.storage().numbering().unwrap(), "opened again");
        assert_eq!(docs(&replica), 6);
        // A transaction of a term older than its last's is still refused.
        let stale = Db::create(&path_for(dir.path(), "s", layout), schema(), opts.clone()).unwrap();
        stale.storage().start_seq_no().unwrap();
        for i in 0..7 {
            upsert_one(&stale, i);
        }
        let log = txns(stale.storage().log_from(7, usize::MAX));
        let e = replica.storage().apply_shipped(last(&replica), &[&*log[0].bytes]).unwrap_err().to_string();
        assert!(e.contains("older than term 4"), "{e}");
        assert_eq!(last(&replica), SeqNo { term: 4, seq: 6 });
        drop((primary, replica, stale));
        std::fs::remove_dir_all(dir.path()).ok();
        std::fs::create_dir_all(dir.path()).unwrap();
    }
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

/// A replica the primary's log no longer reaches takes a copy of the
/// primary (a checkpoint) in place of its rows, numbers and terms, in
/// either layout, and then applies what the primary ships from the copy's
/// last transaction on. Snapshots taken before read what they read, the
/// replica's own log is emptied, and a merge building meanwhile installs
/// nothing; a copy of other tables, of an older term or not numbered is
/// refused.
#[test]
fn a_replica_takes_a_copy_of_its_primary() {
    for (seed, pl, rl) in [(4, Layout::SingleFile, Layout::Directory), (5, Layout::Directory, Layout::SingleFile)] {
        let dir = tempfile::tempdir().unwrap();
        let (pp, rp) = (path_for(dir.path(), "primary", pl), path_for(dir.path(), "replica", rl));
        let ro = options(rl);
        let primary = Db::create(&pp, schema(), options(pl)).unwrap();
        let mut replica = Db::create(&rp, schema(), ro.clone()).unwrap();
        for db in [&primary, &replica] {
            db.create_table(other()).unwrap();
            db.storage().start_seq_no().unwrap();
        }
        let mut rng = StdRng::seed_from_u64(seed);
        for _ in 0..60 {
            write(&primary, &mut rng, 200);
        }
        ship(&primary, &replica, usize::MAX);
        // The replica falls behind: flushed and merged on both sides, a
        // new term, values set in place and deleted rows in segments, and
        // more in the memtables.
        replica.storage().flush().unwrap();
        for _ in 0..60 {
            write(&primary, &mut rng, 200);
        }
        primary.storage().flush().unwrap();
        primary.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
        primary.storage().begin_term(2).unwrap();
        for _ in 0..40 {
            write(&primary, &mut rng, 200);
        }
        primary.storage().flush().unwrap();
        for _ in 0..20 {
            write(&primary, &mut rng, 200);
        }
        for _ in 0..20 {
            write(&replica, &mut rng, 200);
        }
        replica.storage().flush().unwrap();
        // Its own transactions end at the primary's last number, which its
        // log would run on from if it kept it.
        while last(&replica).seq < last(&primary).seq {
            write(&replica, &mut rng, 200);
        }
        let before = replica.snapshot();
        let read_before: Vec<Document> = replica.documents(&before).map(|d| d.unwrap()).collect();
        assert_ne!(documents(&replica), documents(&primary));

        let to = path_for(dir.path(), "copy", Layout::SingleFile);
        primary.storage().checkpoint(&to, &CheckpointOptions::default()).unwrap();
        let copy = Db::open(&to, options(Layout::SingleFile)).unwrap();
        // Installed while a merge of the replica's segments is building.
        let mut installed = None;
        replica
            .storage()
            .merge_with(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }, || {
                installed.get_or_insert_with(|| replica.storage().install_copy(copy.storage()).unwrap());
                true
            })
            .unwrap();
        assert_eq!(installed, Some(last(&primary)), "seed {seed}: a merge built, and the copy was installed meanwhile");
        assert_eq!(replica.snapshot().seq_no(), Some(last(&primary).seq), "seed {seed}: its snapshots hold the copy's last");
        same(&primary, &replica, &format!("seed {seed}: copied"));
        assert_eq!(replica.documents(&before).map(|d| d.unwrap()).collect::<Vec<_>>(), read_before, "a snapshot from before reads what it read");
        let at = last(&replica).seq;
        assert_eq!(replica.storage().log_from(at, usize::MAX), LogRead::Gone { oldest: at + 1 }, "its own transactions are not shipped on");

        // It follows from the copy, and is the same opened again.
        for _ in 0..30 {
            write(&primary, &mut rng, 200);
        }
        assert!(ship(&primary, &replica, 2_000) > 0);
        same(&primary, &replica, &format!("seed {seed}: shipped after the copy"));
        assert_eq!(replica.storage().log_from(1, usize::MAX), LogRead::Gone { oldest: at + 1 }, "its log starts at the copy");
        drop((before, replica));
        replica = Db::open(&rp, ro.clone()).unwrap();
        same(&primary, &replica, &format!("seed {seed}: opened again"));
        replica.storage().check().unwrap();
        write(&primary, &mut rng, 200);
        ship(&primary, &replica, usize::MAX);
        same(&primary, &replica, &format!("seed {seed}: and shipped to"));

        // Refused, changing nothing.
        let refused = |copy: &Db, why: &str| {
            let e = replica.storage().install_copy(copy.storage()).unwrap_err().to_string();
            assert!(e.contains(why), "{e}");
            same(&primary, &replica, why);
        };
        let lone = Db::create(&dir.path().join("lone.strata"), other(), options(Layout::SingleFile)).unwrap();
        lone.storage().start_seq_no().unwrap();
        refused(&lone, "the copy's first table (u) is not this database's (t): neither one ALTER TABLE could make of it nor one of the same columns");
        let lone = Db::create(&dir.path().join("lone2.strata"), schema(), options(Layout::SingleFile)).unwrap();
        lone.storage().start_seq_no().unwrap();
        refused(&lone, "the copy is of term 1, older than this database's (2)");
        let plain = Db::create(&dir.path().join("plain.strata"), schema(), options(Layout::SingleFile)).unwrap();
        plain.create_table(other()).unwrap();
        refused(&plain, "the copy does not number its writes");
        refused(&replica, "not a copy of itself");

        // What a copy holds in its memtables comes too.
        let to = path_for(dir.path(), "copy2", Layout::SingleFile);
        primary.storage().checkpoint(&to, &CheckpointOptions::default()).unwrap();
        let copy = Db::open(&to, options(Layout::SingleFile)).unwrap();
        for _ in 0..5 {
            write(&copy, &mut rng, 200);
        }
        assert_eq!(replica.storage().install_copy(copy.storage()).unwrap(), last(&copy));
        same(&copy, &replica, &format!("seed {seed}: a copy with rows in its memtables"));
    }
}

/// A third table, which references the second.
fn third() -> Schema {
    let mut uid = Field::new("uid", FieldType::Int);
    uid.references = Some("u".into());
    Schema::new("v", vec![Field::new("id", FieldType::Keyword), uid], Some("id")).unwrap()
}

fn upsert_v(db: &Db, i: u64) {
    let mut tx = db.storage().begin().unwrap();
    tx.table("v").unwrap().upsert(vec![Value::Str(key(i)), Value::Int(i as i64)]).unwrap();
    tx.commit().unwrap();
}

fn rows_of(db: &Db, table: &str) -> Vec<Vec<Value>> {
    let mut rows = db.query(&format!("SELECT * FROM {table} LIMIT 100000"), &Params::new()).unwrap().rows;
    rows.sort_by_key(|r| format!("{:?}", r[0]));
    rows
}

/// On a primary that numbers its writes, creating or dropping tables is a
/// numbered transaction that it ships as it ships a write: a replica makes
/// the same change, with the same table ids, so that the writes after it
/// find their tables, shipped in one batch with it or one at a time. A drop
/// with CASCADE removes the references on both. A replica that has a table
/// the primary creates refuses the batch. Opened again, the primary's log
/// starts after its last change of tables, and a replica that its log no
/// longer reaches takes a copy, which brings the primary's tables and ids
/// in place of its own.
#[test]
fn a_replica_makes_the_tables_its_primary_makes() {
    for (pl, rl) in [(Layout::SingleFile, Layout::Directory), (Layout::Directory, Layout::SingleFile)] {
        let dir = tempfile::tempdir().unwrap();
        let (pp, rp) = (path_for(dir.path(), "primary", pl), path_for(dir.path(), "replica", rl));
        let mut primary = Db::create(&pp, schema(), options(pl)).unwrap();
        let replica = Db::create(&rp, schema(), options(rl)).unwrap();
        for db in [&primary, &replica] {
            db.storage().start_seq_no().unwrap();
        }
        let mut rng = StdRng::seed_from_u64(11);
        for i in 0..20 {
            upsert_one(&primary, i);
        }
        primary.create_table(other()).unwrap();
        assert_eq!(last(&primary).seq, 21, "{pl:?}: the table takes the next number");
        for _ in 0..30 {
            write(&primary, &mut rng, 50);
        }
        // The rows before, the table and the rows after, in one batch.
        let n = last(&primary).seq as usize;
        assert_eq!(ship(&primary, &replica, usize::MAX), n);
        same(&primary, &replica, "created");
        assert_eq!(replica.catalog(), primary.catalog());

        // One at a time: a table that references u, rows in it, u dropped
        // with CASCADE (v's reference goes), and u made again, with a new id.
        primary.create_table(third()).unwrap();
        for i in 0..5 {
            upsert_v(&primary, i);
        }
        primary.storage().drop_tables(&["u"], DropOptions { cascade: true, if_exists: false }).unwrap();
        assert_eq!(primary.storage().table("v").unwrap().fields[1].references, None);
        primary.create_table(other()).unwrap();
        let made = last(&primary).seq;
        for _ in 0..20 {
            write(&primary, &mut rng, 50);
        }
        assert!(ship(&primary, &replica, 1) >= 8);
        same(&primary, &replica, "dropped and made again");
        assert_eq!(replica.catalog(), primary.catalog());
        assert_eq!(rows_of(&replica, "v"), rows_of(&primary, "v"));
        replica.storage().check().unwrap();

        // A database that made a table of that name itself is no copy.
        let other_dir = tempfile::tempdir().unwrap();
        let p2 = Db::create(&other_dir.path().join("p2.strata"), schema(), options(Layout::SingleFile)).unwrap();
        let x = Db::create(&other_dir.path().join("x.strata"), schema(), options(Layout::SingleFile)).unwrap();
        for db in [&p2, &x] {
            db.storage().start_seq_no().unwrap();
        }
        upsert_one(&p2, 1);
        p2.create_table(other()).unwrap();
        x.create_table(other()).unwrap();
        let got = txns(p2.storage().log_from(2, usize::MAX));
        let e = x.storage().apply_shipped(prev(&p2, 1), &[&*got[0].bytes]).unwrap_err().to_string();
        assert!(e.contains("shipped transaction 2 creates table u with id 1, which this database has or cannot have"), "{e}");
        assert_eq!(last(&x).seq, 1, "and applies nothing");

        // Opened again, the primary numbers on; its log starts after the
        // last change of tables.
        let at = last(&primary);
        drop(primary);
        primary = Db::open(&pp, options(pl)).unwrap();
        assert_eq!(last(&primary), at);
        assert_eq!(primary.storage().log_from(made, usize::MAX), LogRead::Gone { oldest: made + 1 });
        upsert_one(&primary, 3);
        assert_eq!(ship(&primary, &replica, usize::MAX), 1);
        same(&primary, &replica, "opened again");

        // A replica with a table of its own, at id 1, takes a copy: it then
        // has the primary's tables, with the primary's ids, and applies the
        // writes shipped after the copy.
        let r2 = Db::create(&path_for(dir.path(), "r2", rl), schema(), options(rl)).unwrap();
        r2.storage().start_seq_no().unwrap();
        r2.create_table(Schema::new("w", vec![Field::new("id", FieldType::Int)], Some("id")).unwrap()).unwrap();
        let to = path_for(dir.path(), "copy", Layout::SingleFile);
        primary.storage().checkpoint(&to, &CheckpointOptions::default()).unwrap();
        let copy = Db::open(&to, options(Layout::SingleFile)).unwrap();
        assert_eq!(r2.install_copy(copy.storage()).unwrap(), last(&primary));
        assert_eq!(r2.catalog(), primary.catalog());
        same(&primary, &r2, "copied");
        for _ in 0..20 {
            write(&primary, &mut rng, 50);
        }
        upsert_v(&primary, 9);
        assert!(ship(&primary, &r2, usize::MAX) > 0);
        same(&primary, &r2, "shipped after the copy");
        assert_eq!(rows_of(&r2, "v"), rows_of(&primary, "v"));
        r2.storage().check().unwrap();
        drop(r2);
        let r2 = Db::open(&path_for(dir.path(), "r2", rl), options(rl)).unwrap();
        same(&primary, &r2, "the copy opened again");
        assert_eq!(r2.catalog(), primary.catalog());
    }
}

/// A replica keeps the vectors its primary pooled: rows pooled by a factor
/// at the primary are not pooled again where they are replayed.
#[test]
fn a_replica_keeps_the_vectors_its_primary_pooled() {
    let dir = tempfile::tempdir().unwrap();
    let schema = || {
        let patches = Field::new("patches", FieldType::Vector { dim: 2, index: strata::VectorIndexKind::Flat }).multi().keep_f32().pooled_by(2.0);
        Schema::new("t", vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Keyword), patches], Some("id")).unwrap()
    };
    let primary = Db::create(&dir.path().join("p.strata"), schema(), options(Layout::SingleFile)).unwrap();
    let replica = Db::create(&dir.path().join("r.strata"), schema(), options(Layout::SingleFile)).unwrap();
    for db in [&primary, &replica] {
        db.storage().start_seq_no().unwrap();
    }
    let v = |x: f32, y: f32| Value::Vector(vec![x, y].into());
    let eight = Value::List(vec![v(1.0, 0.0), v(1.0, 0.01), v(0.0, 1.0), v(0.01, 1.0), v(-1.0, 0.0), v(-1.0, 0.01), v(0.0, -1.0), v(0.01, -1.0)]);
    let mut tx = primary.storage().begin().unwrap();
    tx.insert(vec![Value::Str("a".into()), Value::Str("one".into()), eight]).unwrap();
    tx.commit_seq().unwrap();
    primary.sql("UPDATE t SET title = 'two' WHERE id = 'a'", &Params::new()).unwrap();
    ship(&primary, &replica, usize::MAX);
    let stored = |db: &Db| match &db.query("SELECT patches FROM t WHERE id = 'a'", &Params::new()).unwrap().rows[0][0] {
        Value::List(xs) => xs.len(),
        v => panic!("{v:?}"),
    };
    assert_eq!(stored(&primary), 4);
    assert_eq!(stored(&replica), 4, "the replica holds the primary's pooled vectors");
    let (p, r) = (documents_of(&primary), documents_of(&replica));
    assert_eq!(p, r);
}

fn documents_of(db: &Db) -> Vec<Document> {
    db.documents(&db.snapshot()).map(|d| d.unwrap()).collect()
}

fn run(db: &Db, sql: &str) {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn select(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")).rows
}

/// An `ALTER TABLE` on a primary that numbers its writes is a numbered
/// transaction that it ships as it ships a write: a replica gives the
/// table the same columns under the same field ids, so that the writes
/// after it, shipped in one batch with it or one at a time, fill the same
/// columns, and the rows from before read the same defaults; opened
/// again, it holds them. A database whose table has the same columns by
/// other changes (other field ids) refuses the change. A replica made
/// before the changes takes a copy of the primary, as does one made since
/// with the table's columns as they are now; one whose first table is
/// neither does not.
#[test]
fn a_replica_alters_the_tables_its_primary_alters() {
    for (pl, rl) in [(Layout::SingleFile, Layout::Directory), (Layout::Directory, Layout::SingleFile)] {
        let dir = tempfile::tempdir().unwrap();
        let (pp, rp) = (path_for(dir.path(), "primary", pl), path_for(dir.path(), "replica", rl));
        let primary = Db::create(&pp, schema(), options(pl)).unwrap();
        let mut replica = Db::create(&rp, schema(), options(rl)).unwrap();
        for db in [&primary, &replica] {
            db.create_table(other()).unwrap();
            db.storage().start_seq_no().unwrap();
        }
        let mut rng = StdRng::seed_from_u64(12);
        for _ in 0..40 {
            write(&primary, &mut rng, 50);
        }
        primary.storage().flush().unwrap();
        for _ in 0..10 {
            write(&primary, &mut rng, 50);
        }
        assert!(ship(&primary, &replica, usize::MAX) > 0);

        // Two columns in one change, which takes the next number, then rows
        // and values set in place in them, all in one batch.
        let at = last(&primary).seq;
        run(&primary, "ALTER TABLE t ADD COLUMN tag KEYWORD DEFAULT 'old', ADD COLUMN w FLOAT");
        assert_eq!(last(&primary).seq, at + 1, "{pl:?}: the change takes the next number");
        run(&primary, "INSERT INTO t (id, n, body, tag, w) VALUES ('z1', 1, 'written after', 'new', 0.5), ('z2', 2, 'also after', NULL, 1.5)");
        run(&primary, "UPDATE t SET tag = 'set' WHERE n < 300");
        run(&primary, "ALTER TABLE u ADD COLUMN label KEYWORD");
        run(&primary, "INSERT INTO u (id, x, label) VALUES (5000, 1.5, 'l')");
        assert_eq!(ship(&primary, &replica, usize::MAX), 5);
        same(&primary, &replica, "added");
        assert_eq!(replica.catalog(), primary.catalog());
        let olds = "SELECT count(*) FROM t WHERE tag = 'old'";
        assert!(select(&primary, olds)[0][0].as_i64().unwrap() > 0, "rows from before read the default");
        assert_eq!(select(&replica, olds), select(&primary, olds));
        assert_eq!(select(&replica, "SELECT label FROM u WHERE id = 5000"), [[Value::Str("l".into())]]);

        // Renamed and dropped, one transaction at a time.
        run(&primary, "ALTER TABLE t RENAME COLUMN body TO text");
        run(&primary, "UPDATE t SET text = 'changed after the rename' WHERE id = 'z1'");
        run(&primary, "ALTER TABLE t DROP COLUMN w");
        run(&primary, "INSERT INTO t (id, n, text, tag) VALUES ('z3', 3, 'after the drop', 'new')");
        assert_eq!(ship(&primary, &replica, 1), 4);
        same(&primary, &replica, "renamed and dropped");
        assert_eq!(replica.catalog(), primary.catalog());
        let z = "SELECT id, text, tag FROM t WHERE id IN ('z1', 'z3') ORDER BY id";
        assert_eq!(select(&replica, z), select(&primary, z));
        replica.storage().check().unwrap();

        // Opened again, the replica holds the columns, and follows on.
        drop(replica);
        replica = Db::open(&rp, options(rl)).unwrap();
        assert_eq!(replica.catalog(), primary.catalog());
        run(&primary, "INSERT INTO t (id, n, text, tag) VALUES ('z4', 4, 'after reopening', 'new')");
        assert_eq!(ship(&primary, &replica, usize::MAX), 1);
        same(&primary, &replica, "opened again");

        // The same columns, made by other changes, are not the primary's
        // table: their field ids differ.
        let other_dir = tempfile::tempdir().unwrap();
        let p2 = Db::create(&other_dir.path().join("p2.strata"), schema(), options(Layout::SingleFile)).unwrap();
        let mut wider = schema().fields;
        wider.push(Field::new("junk", FieldType::Int));
        let x = Db::create(&other_dir.path().join("x.strata"), Schema::new("t", wider, Some("id")).unwrap(), options(Layout::SingleFile)).unwrap();
        run(&x, "ALTER TABLE t DROP COLUMN junk");
        assert!(x.schema().same_columns(&p2.schema()));
        for db in [&p2, &x] {
            db.storage().start_seq_no().unwrap();
        }
        run(&p2, "ALTER TABLE t ADD COLUMN tag KEYWORD");
        let got = txns(p2.storage().log_from(1, usize::MAX));
        let e = x.storage().apply_shipped(prev(&p2, 0), &[&*got[0].bytes]).unwrap_err().to_string();
        assert!(e.contains("shipped transaction 1 alters the table with id 0, which this database lacks, cannot alter, or holds with other columns or field ids"), "{e}");
        assert_eq!(last(&x).seq, 0, "and applies nothing");
        assert!(x.schema().field("tag").is_none());

        // Copies: to a replica made before the changes, and to one made
        // since with the table's columns (under new field ids); both then
        // hold the primary's ids, and follow on.
        let to = path_for(dir.path(), "copy", Layout::SingleFile);
        primary.storage().checkpoint(&to, &CheckpointOptions::default()).unwrap();
        let copy = Db::open(&to, options(Layout::SingleFile)).unwrap();
        let now = primary.storage().table("t").unwrap();
        let cols = now.fields.iter().cloned().map(|mut f| {
            f.id = strata::schema::NO_ID;
            f.added = None;
            f
        });
        let fresh = Schema::new("t", cols.collect(), Some("id")).unwrap();
        assert!(fresh.same_columns(&now) && fresh != *now);
        let mut copies = Vec::new();
        for (name, first) in [("r2", schema()), ("r3", fresh)] {
            let r = Db::create(&path_for(dir.path(), name, rl), first, options(rl)).unwrap();
            r.storage().start_seq_no().unwrap();
            assert_eq!(r.install_copy(copy.storage()).unwrap(), last(&primary), "{name}");
            assert_eq!(r.catalog(), primary.catalog(), "{name}");
            same(&primary, &r, name);
            copies.push(r);
        }
        run(&primary, "INSERT INTO t (id, n, text, tag) VALUES ('z5', 5, 'after the copies', 'new')");
        for r in &copies {
            assert_eq!(ship(&primary, r, usize::MAX), 1);
            same(&primary, r, "shipped after a copy");
            r.storage().check().unwrap();
        }
        let odd = Schema::new("t", vec![Field::new("id", FieldType::Keyword), Field::new("n", FieldType::Float)], Some("id")).unwrap();
        let r = Db::create(&path_for(dir.path(), "odd", rl), odd, options(rl)).unwrap();
        r.storage().start_seq_no().unwrap();
        let e = r.install_copy(copy.storage()).unwrap_err().to_string();
        assert!(e.contains("the copy's first table (t) is not this database's (t): neither one ALTER TABLE could make of it nor one of the same columns"), "{e}");
    }
}

/// Ships what `replica` lacks of `primary`'s log in one batch, through
/// `Db::apply_shipped` (which keeps the replica's models); returns how
/// many transactions it shipped.
fn ship_to_db(primary: &Db, replica: &Db) -> usize {
    let at = last(replica);
    let got = txns(primary.storage().log_from(at.seq + 1, usize::MAX));
    let bytes: Vec<&[u8]> = got.iter().map(|t| &*t.bytes).collect();
    replica.apply_shipped(prev(primary, at.seq), &bytes).unwrap();
    got.len()
}

/// A replica's models follow the columns of a table its primary altered,
/// as the primary's do: a renamed embedded column keeps its model (the
/// same one, not opened again), an added one gets its own, and a dropped
/// one loses it.
#[test]
fn a_replica_s_models_follow_its_primary_s_columns() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    let primary = Db::create(&dir.path().join("p.strata"), schema(), options(Layout::SingleFile)).unwrap();
    let replica = Db::create(&dir.path().join("r.strata"), schema(), options(Layout::SingleFile)).unwrap();
    for db in [&primary, &replica] {
        run(db, "ALTER TABLE t ADD COLUMN emb VECTOR OPTIONS(embed = 'body', model = 'hash:8', index = 'flat')");
        assert!(db.open_embedders(&models).is_empty());
        db.storage().start_seq_no().unwrap();
    }
    assert_eq!(replica.catalog(), primary.catalog());
    let model = replica.embedder_in("t", "emb").unwrap();
    run(&primary, "INSERT INTO t (id, n, body) VALUES ('a', 1, 'red apple')");
    run(&primary, "ALTER TABLE t RENAME COLUMN emb TO vec, RENAME COLUMN body TO text");
    run(&primary, "ALTER TABLE t ADD COLUMN emb2 VECTOR OPTIONS(embed = 'text', model = 'hash:4', index = 'flat')");
    assert_eq!(ship_to_db(&primary, &replica), 3);
    assert_eq!(replica.catalog(), primary.catalog());
    assert!(Arc::ptr_eq(&replica.embedder_in("t", "vec").unwrap(), &model), "the renamed column keeps its model");
    assert!(replica.embedder_in("t", "emb").is_none());
    assert_eq!(replica.embedder_in("t", "emb2").map(|m| m.dim()), Some(4));
    let q = "SELECT vec FROM t WHERE id = 'a'";
    assert_eq!(select(&replica, q), select(&primary, q));
    run(&primary, "ALTER TABLE t DROP COLUMN emb2");
    assert_eq!(ship_to_db(&primary, &replica), 1);
    assert!(replica.embedder_in("t", "emb2").is_none());
    assert!(Arc::ptr_eq(&replica.embedder_in("t", "vec").unwrap(), &model));
}
