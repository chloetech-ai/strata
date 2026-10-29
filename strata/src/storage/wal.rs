//! Write-ahead log: the `<path>-wal` sidecar.
//!
//! ```text
//! header:  "STRWAL01" | db_id: u64
//!     or   "STRWAL02" | db_id: u64 | len: u32 | features, comma-separated
//! record:  len: u32 | crc32(body): u32 | body = kind: u8, payload
//!          kind 1 insert: document     kind 2 delete: key value
//!          kind 3 commit: txn: u64, n_ops: u32
//!                 (STRWAL02: then term: u64, seq: u64)
//!          kind 4 set: key value, field: u16, value
//!          kind 5 table: id: u32 (the operations up to the next table
//!                 record or the commit are on this table; before any,
//!                 on table 0)
//! ```
//!
//! A database that numbers its writes
//! ([`crate::storage::Database::start_seq_no`]) logs in `STRWAL02`, whose
//! header names the features its records use (`seq_no`: each commit
//! carries the transaction's term and sequence number); a reader refuses a
//! log naming one it does not know. Others log in `STRWAL01`, as before.
//!
//! A transaction is its operation records followed by a commit record,
//! written with one `write` and made durable with one `fdatasync`. Replay
//! stops at the first record that is short, fails its checksum or holds an
//! operation its table's schema could not have committed (damage the
//! checksum missed), drops operations not followed by a commit, and the
//! log is cut there. If
//! commit records follow the damage, the log is first copied aside
//! (`<wal>.damaged`), and with `sync` more than one of them means the log
//! was damaged after it was durable, not torn by a crash during the last
//! append: opening then fails rather than drop them, unless the caller asks
//! to ([`Wal::open`]'s `drop_damaged`). The WAL is truncated after each
//! flush; transactions a crash left behind in it are skipped by txn (see
//! `Manifest::wal_txn`).
//!
//! A background flush seals the log instead: it is made durable and
//! renamed to `<wal>.<txn>` ([`sealed_path`]: it holds transactions up to
//! that txn), and commits go on in a new, empty log while the flush builds
//! its segments. Opening replays the sealed logs a flush did not install,
//! oldest first, before the open one, and deletes the others. A sealed log
//! was durable before it got its name, so damage in one is damage after
//! the fact: opening fails, or with `drop_damaged` cuts the log there and
//! drops every later transaction, in it and in the logs after it, once
//! each is copied aside.
//!
//! Values are encoded as a tag byte and a little-endian payload: integers and
//! floats in 8 bytes, strings and vectors as a u32 length and the bytes (or
//! f32s).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{corrupt, Error, Result};
use crate::storage::io;
use crate::storage::manifest::SeqNo;
use crate::value::{Document, Value};

const MAGIC: &[u8; 8] = b"STRWAL01";
/// A log whose header names its features.
const MAGIC_2: &[u8; 8] = b"STRWAL02";
/// The features a log may name, all of which this version reads.
const FEATURES: &[&str] = &["seq_no"];

const INSERT: u8 = 1;
const DELETE: u8 = 2;
const COMMIT: u8 = 3;
const SET: u8 = 4;
const TABLE: u8 = 5;

/// One logged operation.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Insert(Arc<Document>),
    Delete(Value),
    /// Sets a numeric field of the live document with this key, in place
    /// ([`crate::storage::updates`]).
    Set { key: Value, field: u16, value: Value },
}

/// A committed transaction read back from the log: its sequence number
/// (in a numbered log), and its operations, each with the id of the table
/// it writes.
#[derive(Debug, PartialEq)]
pub struct WalTxn {
    pub txn: u64,
    pub seq: Option<SeqNo>,
    pub ops: Vec<(u32, Op)>,
}

/// The open log, appended to by the single writer.
pub struct Wal {
    file: File,
    db_id: u64,
    /// Whether its transactions carry sequence numbers (`STRWAL02`).
    numbered: bool,
    /// The header's length.
    header: u64,
    len: u64,
    /// What opening cut off past damage that committed transactions follow.
    dropped: Option<String>,
    /// Changed since its last sync (appends without `sync`).
    unsynced: bool,
}

/// What a log's header says: whether its transactions are numbered, and
/// the header's length. `None` for a file that is no log of database
/// `db_id` (or whose header a crash cut short); an error for a log that
/// uses a feature this version does not have.
fn header(buf: &[u8], db_id: u64, path: &Path) -> Result<Option<(bool, u64)>> {
    if buf.len() < 16 || buf[8..16] != db_id.to_le_bytes() {
        return Ok(None);
    }
    if &buf[..8] == MAGIC {
        return Ok(Some((false, 16)));
    }
    if &buf[..8] != MAGIC_2 {
        return Ok(None);
    }
    let Some(n) = buf.get(16..20).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize) else { return Ok(None) };
    let Some(names) = buf.get(20..20 + n) else { return Ok(None) };
    let names = std::str::from_utf8(names).map_err(|_| corrupt(format!("{}: the header's features are not text", path.display())))?;
    if let Some(x) = names.split(',').find(|x| !FEATURES.contains(x)) {
        return Err(Error::Schema(format!("the write-ahead log {} uses feature {x:?}, which this version of Strata does not have: open it with a newer one", path.display())));
    }
    Ok(Some((true, 20 + n as u64)))
}

/// The header of a log of database `db_id`: numbered or not.
fn header_bytes(db_id: u64, numbered: bool) -> Vec<u8> {
    let mut h = if numbered { MAGIC_2.to_vec() } else { MAGIC.to_vec() };
    h.extend_from_slice(&db_id.to_le_bytes());
    if numbered {
        let names = FEATURES.join(",");
        h.extend_from_slice(&(names.len() as u32).to_le_bytes());
        h.extend_from_slice(names.as_bytes());
    }
    h
}

impl Wal {
    /// Creates (or truncates) the log, numbered (`STRWAL02`) or not.
    pub fn create(path: &Path, db_id: u64, numbered: bool, sync: bool) -> Result<Wal> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        let mut wal = Wal { file, db_id, numbered, header: 0, len: 0, dropped: None, unsynced: true };
        wal.reset(sync)?;
        Ok(wal)
    }

    /// Opens the log and returns its committed transactions. A missing log,
    /// or one left by another database (different id), counts as empty. A
    /// torn tail is cut off so that later appends follow the last commit.
    /// Committed transactions after the damage are dropped too, once the
    /// whole log is copied aside ([`damaged_path`]). With `sync` that is
    /// one at most, the last append, which had not returned; more mean the
    /// log was damaged after it was made durable, and opening fails unless
    /// `drop_damaged`. (Without `sync` a power loss can tear any appends
    /// not yet written back.)
    /// `fits(table, op)` says whether an operation could have been
    /// committed; one that could not counts as damage.
    ///
    /// `numbered` says whether the database numbers its writes. A log that
    /// does otherwise is started again in the database's kind when it holds
    /// no transaction, and refused when it does.
    pub fn open(path: &Path, db_id: u64, numbered: bool, sync: bool, drop_damaged: bool, fits: &dyn Fn(u32, &Op) -> bool) -> Result<(Wal, Vec<WalTxn>)> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let size = file.metadata()?.len();
        let mut buf = vec![0u8; size as usize];
        file.read_exact_at(&mut buf, 0)?;
        // What an earlier process appended may not be durable yet.
        let mut wal = Wal { file, db_id, numbered, header: 0, len: size, dropped: None, unsynced: true };
        let Some((logged, header)) = header(&buf, db_id, path)? else {
            wal.reset(sync)?;
            return Ok((wal, Vec::new()));
        };
        let (txns, end) = decode(&buf[header as usize..], logged, fits);
        if logged != numbered {
            if !txns.is_empty() {
                return Err(corrupt(format!(
                    "{} holds transactions {} sequence numbers, and the database {}",
                    path.display(),
                    if logged { "with" } else { "without" },
                    if numbered { "numbers its writes" } else { "does not number its writes" }
                )));
            }
            wal.reset(sync)?;
            return Ok((wal, Vec::new()));
        }
        wal.header = header;
        let end = header + end as u64;
        if end < size {
            let later = commits_after(&buf[end as usize..], txns.last().map_or(0, |t| t.txn), numbered);
            let saved = damaged_path(path);
            if !later.is_empty() {
                let what = format!(
                    "{} is damaged at byte {end}, and {} committed transactions follow the damage (up to txn {})",
                    path.display(),
                    later.len(),
                    later.iter().max().unwrap()
                );
                if sync && later.len() > 1 && !drop_damaged {
                    return Err(corrupt(format!("{what}; open with DbOptions::wal_drop_damaged to drop them (the log is copied to {} first)", saved.display())));
                }
                std::fs::write(&saved, &buf)?;
                wal.dropped = Some(format!("{what}: dropped them, the whole log is copied to {}", saved.display()));
            }
            io::set_len(&wal.file, end)?;
            if sync {
                io::sync(&wal.file)?;
            }
            wal.len = end;
        }
        Ok((wal, txns))
    }

    /// Set when opening dropped committed transactions after damage: where
    /// the log was damaged, what was dropped, where the copy is.
    pub fn dropped(&self) -> Option<&str> {
        self.dropped.as_deref()
    }

    /// Appends a committed transaction (operations with the id of the table
    /// each writes) and its sequence number, which a numbered log needs and
    /// any other refuses; durable on return when `sync`. Returns the
    /// records it wrote.
    pub fn append(&mut self, txn: u64, seq: Option<SeqNo>, ops: &[(u32, Op)], sync: bool) -> Result<Vec<u8>> {
        match (seq.is_some(), self.numbered) {
            (true, false) => return Err(Error::Schema("a numbered transaction, in a log without sequence numbers".into())),
            (false, true) => return Err(Error::Schema("a transaction without a sequence number, in a numbered log".into())),
            _ => {}
        }
        let buf = encode_txn(txn, seq, ops);
        let res = io::write_at(&self.file, &buf, self.len).and_then(|_| if sync { io::sync(&self.file) } else { Ok(()) });
        if let Err(e) = res {
            // Cut off whatever part landed, so that later commits stay
            // reachable by replay; if even that fails the caller poisons
            // the database.
            io::set_len(&self.file, self.len)?;
            return Err(e.into());
        }
        self.len += buf.len() as u64;
        self.unsynced |= !sync;
        Ok(buf)
    }

    /// Makes appends made without `sync` durable.
    pub fn sync_appends(&mut self) -> Result<()> {
        if self.unsynced {
            io::sync(&self.file)?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// Empties the log (after a flush made its contents durable elsewhere).
    pub fn reset(&mut self, sync: bool) -> Result<()> {
        io::set_len(&self.file, 0)?;
        let h = header_bytes(self.db_id, self.numbered);
        io::write_at(&self.file, &h, 0)?;
        if sync {
            io::sync(&self.file)?;
        }
        self.unsynced = !sync;
        self.header = h.len() as u64;
        self.len = self.header;
        Ok(())
    }

    /// Empties the log and starts it again numbered (`STRWAL02`), for a
    /// database that has just begun numbering its writes.
    pub fn renumber(&mut self, sync: bool) -> Result<()> {
        self.numbered = true;
        self.reset(sync)
    }

    /// Whether its transactions carry sequence numbers.
    pub fn numbered(&self) -> bool {
        self.numbered
    }

    /// Starts an empty log at `path` in place of this one, which the caller
    /// made durable ([`Wal::sync_appends`]) and renamed ([`sealed_path`]).
    pub fn restart(&mut self, path: &Path, sync: bool) -> Result<()> {
        let dropped = self.dropped.take();
        *self = Wal::create(path, self.db_id, self.numbered, sync)?;
        self.dropped = dropped;
        Ok(())
    }

    /// Size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len <= self.header
    }
}

/// The records of one transaction. A database with one table never writes
/// a table record, so its log reads the same as before there were tables.
pub(crate) fn encode_txn(txn: u64, seq: Option<SeqNo>, ops: &[(u32, Op)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut body = Vec::new();
    let mut table = 0;
    for (t, op) in ops {
        if *t != table {
            body.clear();
            body.push(TABLE);
            body.extend_from_slice(&t.to_le_bytes());
            frame(&mut out, &body);
            table = *t;
        }
        body.clear();
        match op {
            Op::Insert(doc) => {
                body.push(INSERT);
                put_doc(&mut body, doc);
            }
            Op::Delete(key) => {
                body.push(DELETE);
                put_value(&mut body, key);
            }
            Op::Set { key, field, value } => {
                body.push(SET);
                put_value(&mut body, key);
                body.extend_from_slice(&field.to_le_bytes());
                put_value(&mut body, value);
            }
        }
        frame(&mut out, &body);
    }
    body.clear();
    body.push(COMMIT);
    body.extend_from_slice(&txn.to_le_bytes());
    body.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    if let Some(n) = seq {
        body.extend_from_slice(&n.term.to_le_bytes());
        body.extend_from_slice(&n.seq.to_le_bytes());
    }
    frame(&mut out, &body);
    out
}

fn frame(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(body).to_le_bytes());
    out.extend_from_slice(body);
}

/// Where a damaged log is copied before it is cut.
pub fn damaged_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".damaged");
    p.into()
}

/// The name the log at `path` takes when a background flush seals it at
/// txn `txn`: `<wal>.<txn>`, in 20 digits so that names sort by txn.
pub fn sealed_path(path: &Path, txn: u64) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(format!(".{txn:020}"));
    p.into()
}

/// The sealed logs beside the log at `path`, oldest first, with the txn
/// each was sealed at.
pub fn sealed_files(path: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let file = e.file_name();
        let Some(rest) = file.to_str().and_then(|f| f.strip_prefix(name.as_str())).and_then(|r| r.strip_prefix('.')) else { continue };
        if rest.len() == 20 && rest.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(txn) = rest.parse() {
                found.push((txn, e.path()));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The committed transactions of the sealed log at `path`, and the byte
/// where it is damaged, if it is; `None` for a log of another database
/// (`db_id`) or a file that is not a log. Its transactions carry sequence
/// numbers if it is numbered.
pub fn read_sealed(path: &Path, db_id: u64, fits: &dyn Fn(u32, &Op) -> bool) -> Result<Option<(Vec<WalTxn>, Option<u64>)>> {
    let buf = std::fs::read(path)?;
    let Some((numbered, header)) = header(&buf, db_id, path)? else { return Ok(None) };
    let (txns, end) = decode(&buf[header as usize..], numbered, fits);
    let end = header + end as u64;
    Ok(Some((txns, (end < buf.len() as u64).then_some(end))))
}

/// The one numbered transaction `buf` holds, as [`Wal::append`] wrote it
/// (a primary's, shipped to a replica: [`crate::storage::ship`]); an error
/// if it holds anything else or an operation that does not `fit`.
pub fn decode_shipped(buf: &[u8], fits: &dyn Fn(u32, &Op) -> bool) -> Result<WalTxn> {
    match decode(buf, true, fits) {
        (mut txns, end) if txns.len() == 1 && end == buf.len() => Ok(txns.pop().expect("one")),
        _ => Err(corrupt("a shipped transaction is damaged, or is not one numbered transaction")),
    }
}

/// The txns of the intact commit records in `rest` (the log past the last
/// transaction replay reached) that come after txn `last`. A commit record
/// is 21 bytes (length 13, checksum, kind, txn, op count), or 37 in a
/// numbered log (length 29: then term and sequence number), so finding
/// one in damaged bytes by chance takes a checksum collision.
fn commits_after(rest: &[u8], last: u64, numbered: bool) -> Vec<u64> {
    let body_len = if numbered { 29 } else { 13 };
    let mut found = Vec::new();
    for p in 0..rest.len().saturating_sub(8 + body_len - 1) {
        if u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize != body_len {
            continue;
        }
        let body = &rest[p + 8..p + 8 + body_len];
        if body[0] == COMMIT && crc32fast::hash(body) == u32::from_le_bytes(rest[p + 4..p + 8].try_into().unwrap()) {
            let txn = u64::from_le_bytes(body[1..9].try_into().unwrap());
            if txn > last {
                found.push(txn);
            }
        }
    }
    found
}

/// Committed transactions in `buf` (the log after its header) and the
/// length of the prefix they occupy, up to the first operation that does
/// not `fit`. In a `numbered` log, sequence numbers rise from one
/// transaction to the next and terms never fall; one that does not is
/// damage too.
fn decode(buf: &[u8], numbered: bool, fits: &dyn Fn(u32, &Op) -> bool) -> (Vec<WalTxn>, usize) {
    let mut txns = Vec::new();
    let mut ops = Vec::new();
    let mut table = 0u32;
    let (mut pos, mut end) = (0usize, 0usize);
    while pos + 8 <= buf.len() {
        let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
        let Some(body) = buf.get(pos + 8..pos + 8 + len) else { break };
        if len == 0 || crc32fast::hash(body) != crc {
            break;
        }
        pos += 8 + len;
        let mut r = Reader { b: &body[1..] };
        let op = match body[0] {
            INSERT => get_doc(&mut r).map(|d| Op::Insert(Arc::new(d))),
            DELETE => get_value(&mut r).map(Op::Delete),
            TABLE => match r.u32() {
                Ok(t) => {
                    table = t;
                    continue;
                }
                Err(_) => break,
            },
            SET => {
                let key = get_value(&mut r);
                let field = r.u16();
                let value = get_value(&mut r);
                match (key, field, value) {
                    (Ok(key), Ok(field), Ok(value)) => Ok(Op::Set { key, field, value }),
                    _ => break,
                }
            }
            COMMIT => {
                let (Ok(txn), Ok(n)) = (r.u64(), r.u32()) else { break };
                let seq = match numbered {
                    false => None,
                    true => match (r.u64(), r.u64()) {
                        (Ok(term), Ok(seq)) => Some(SeqNo { term, seq }),
                        _ => break,
                    },
                };
                let last = txns.last();
                if n as usize != ops.len() || last.is_some_and(|t: &WalTxn| t.txn >= txn) {
                    break;
                }
                if let (Some(a), Some(b)) = (last.and_then(|t| t.seq), seq) {
                    if b.seq <= a.seq || b.term < a.term {
                        break;
                    }
                }
                txns.push(WalTxn { txn, seq, ops: std::mem::take(&mut ops) });
                table = 0;
                end = pos;
                continue;
            }
            _ => break,
        };
        match op {
            Ok(op) if fits(table, &op) => ops.push((table, op)),
            _ => break,
        }
    }
    (txns, end)
}

const NULL: u8 = 0;
const INT: u8 = 1;
const FLOAT: u8 = 2;
const STR: u8 = 3;
const VECTOR: u8 = 4;
const LIST: u8 = 5;

fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(NULL),
        // Writes log documents as stored (`value::conform`), where BOOL and
        // TIMESTAMP fields hold integers; any other boolean or timestamp
        // is logged as the same integer.
        Value::Int(x) | Value::Timestamp(x) => {
            out.push(INT);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Bool(b) => {
            out.push(INT);
            out.extend_from_slice(&(*b as i64).to_le_bytes());
        }
        Value::Float(x) => {
            out.push(FLOAT);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Str(s) => {
            out.push(STR);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::List(xs) => {
            out.push(LIST);
            out.extend_from_slice(&(xs.len() as u32).to_le_bytes());
            for x in xs {
                put_value(out, x);
            }
        }
        Value::Vector(x) => {
            out.push(VECTOR);
            out.extend_from_slice(&(x.len() as u32).to_le_bytes());
            out.reserve(4 * x.len());
            for f in x.iter() {
                out.extend_from_slice(&f.to_le_bytes());
            }
        }
    }
}

fn put_doc(out: &mut Vec<u8>, doc: &Document) {
    out.extend_from_slice(&(doc.len() as u32).to_le_bytes());
    for v in doc {
        put_value(out, v);
    }
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.b.len() {
            return Err(corrupt("truncated WAL record"));
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

fn get_value(r: &mut Reader) -> Result<Value> {
    Ok(match r.u8()? {
        NULL => Value::Null,
        INT => Value::Int(r.u64()? as i64),
        FLOAT => Value::Float(f64::from_bits(r.u64()?)),
        STR => {
            let n = r.u32()? as usize;
            Value::Str(String::from_utf8(r.take(n)?.to_vec()).map_err(corrupt)?)
        }
        VECTOR => {
            let n = r.u32()? as usize;
            let b = r.take(n.checked_mul(4).ok_or_else(|| corrupt("vector length"))?)?;
            Value::Vector(b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
        }
        LIST => {
            let n = r.u32()? as usize;
            let mut xs = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                xs.push(get_value(r)?);
            }
            Value::List(xs)
        }
        t => return Err(corrupt(format!("unknown value tag {t}"))),
    })
}

fn get_doc(r: &mut Reader) -> Result<Document> {
    let n = r.u32()? as usize;
    // Bound the allocation by what the record can hold (a value is >= 1 byte).
    let mut doc = Vec::with_capacity(n.min(r.b.len()));
    for _ in 0..n {
        doc.push(get_value(r)?);
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any(_: u32, _: &Op) -> bool {
        true
    }

    fn doc(i: i64) -> Arc<Document> {
        Arc::new(vec![
            Value::Str(format!("key{i}")),
            Value::Int(i),
            Value::Float(i as f64 / 3.0),
            Value::Null,
            Value::Vector(vec![i as f32, -0.5, 1e-3].into()),
        ])
    }

    type Txns = Vec<(u64, Option<SeqNo>, Vec<(u32, Op)>)>;

    /// Transactions without sequence numbers, as a database that does not
    /// number its writes logs them.
    fn sample() -> Txns {
        unnumbered().into_iter().map(|(t, ops)| (t, None, ops)).collect()
    }

    /// The same, numbered: from 5, with a new term from the fourth.
    fn numbered() -> Txns {
        let terms = [1, 1, 1, 2, 2, 7];
        unnumbered().into_iter().enumerate().map(|(i, (t, ops))| (t, Some(SeqNo { term: terms[i], seq: 5 + i as u64 }), ops)).collect()
    }

    fn unnumbered() -> Vec<(u64, Vec<(u32, Op)>)> {
        vec![
            (3, vec![(0, Op::Insert(doc(1))), (0, Op::Insert(doc(2)))]),
            (4, vec![(0, Op::Delete(Value::Str("key1".into())))]),
            (5, vec![]),
            (7, vec![(0, Op::Delete(Value::Int(9))), (0, Op::Insert(doc(3))), (0, Op::Set { key: Value::Int(3), field: 2, value: Value::Float(0.5) })]),
            // Several tables in one transaction; the next one starts on
            // table 0 again.
            (8, vec![(2, Op::Insert(doc(6))), (2, Op::Delete(Value::Int(1))), (0, Op::Insert(doc(7))), (5, Op::Insert(doc(8)))]),
            (9, vec![(0, Op::Insert(doc(9)))]),
        ]
    }

    /// Operations on table 0 alone are logged exactly as before tables.
    #[test]
    fn one_table_writes_no_table_records() {
        let ops = [(0, Op::Insert(doc(1))), (0, Op::Delete(Value::Int(2)))];
        let buf = encode_txn(3, None, &ops);
        let mut want = Vec::new();
        let mut body = vec![INSERT];
        put_doc(&mut body, &doc(1));
        frame(&mut want, &body);
        body = vec![DELETE];
        put_value(&mut body, &Value::Int(2));
        frame(&mut want, &body);
        body = vec![COMMIT];
        body.extend_from_slice(&3u64.to_le_bytes());
        body.extend_from_slice(&2u32.to_le_bytes());
        frame(&mut want, &body);
        assert_eq!(buf, want);
    }

    fn encode(txns: &Txns) -> (Vec<u8>, Vec<usize>) {
        let (mut buf, mut ends) = (Vec::new(), Vec::new());
        for (t, seq, ops) in txns {
            buf.extend(encode_txn(*t, *seq, ops));
            ends.push(buf.len());
        }
        (buf, ends)
    }

    #[test]
    fn roundtrip() {
        for (txns, numbered) in [(sample(), false), (numbered(), true)] {
            let (buf, _) = encode(&txns);
            let (got, end) = decode(&buf, numbered, &any);
            assert_eq!(end, buf.len());
            let want: Vec<WalTxn> = txns.into_iter().map(|(txn, seq, ops)| WalTxn { txn, seq, ops }).collect();
            assert_eq!(got, want);
        }
    }

    /// In a numbered log a sequence number that does not rise, or a term
    /// that falls, is damage: replay stops before it.
    #[test]
    fn sequence_numbers_rise() {
        let mut txns = numbered();
        txns[3].1 = Some(SeqNo { term: 2, seq: 7 });
        let (buf, ends) = encode(&txns);
        assert_eq!(decode(&buf, true, &any), (decode(&buf[..ends[2]], true, &any).0, ends[2]));
        let mut txns = numbered();
        txns[4].1 = Some(SeqNo { term: 1, seq: 9 });
        let (buf, ends) = encode(&txns);
        assert_eq!(decode(&buf, true, &any).1, ends[3]);
        // Gaps are allowed: what the log holds is what was numbered.
        let mut txns = numbered();
        txns[5].1 = Some(SeqNo { term: 7, seq: 100 });
        let (buf, _) = encode(&txns);
        assert_eq!(decode(&buf, true, &any).0.len(), 6);
    }

    /// Cutting the log at every byte (a torn write) and appending garbage
    /// replays exactly the transactions whose records survived intact (a
    /// zero tail can restore zero bytes the cut removed).
    #[test]
    fn torn_tail_at_every_offset() {
        for (txns, numbered) in [(sample(), false), (numbered(), true)] {
            let (buf, ends) = encode(&txns);
            for cut in 0..=buf.len() {
                for tail in [&[][..], &[0u8; 16][..], &[0xffu8; 7][..]] {
                    let mut b = buf[..cut].to_vec();
                    b.extend_from_slice(tail);
                    let committed = ends.iter().filter(|&&e| b.starts_with(&buf[..e])).count();
                    let (txns, end) = decode(&b, numbered, &any);
                    assert_eq!(txns.len(), committed, "cut at {cut}");
                    assert_eq!(end, if committed == 0 { 0 } else { ends[committed - 1] });
                }
            }
            // A flipped bit inside the second transaction loses it and the rest.
            let mut b = buf.clone();
            b[ends[0] + 12] ^= 4;
            assert_eq!(decode(&b, numbered, &any).0.len(), 1);
        }
    }

    #[test]
    fn file_append_reopen_and_foreign_log() {
        for (txns, numbered) in [(sample(), false), (numbered(), true)] {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("db-wal");
            let mut wal = Wal::create(&p, 11, numbered, false).unwrap();
            for (t, seq, ops) in &txns {
                wal.append(*t, *seq, ops, true).unwrap();
            }
            let len = wal.len();
            drop(wal);
            assert_eq!(&std::fs::read(&p).unwrap()[..8], if numbered { b"STRWAL02" } else { b"STRWAL01" });
            // Torn tail: half a record after the last commit is cut on open.
            let next = numbered.then_some(SeqNo { term: 7, seq: 11 });
            let f = OpenOptions::new().write(true).open(&p).unwrap();
            f.write_all_at(&encode_txn(9, next, &[(0, Op::Insert(doc(4)))])[..20], len).unwrap();
            let (mut wal, got) = Wal::open(&p, 11, numbered, false, false, &any).unwrap();
            assert_eq!(got.len(), 6);
            assert_eq!(got.last().unwrap().seq, txns.last().unwrap().1);
            assert_eq!(wal.len(), len);
            // A transaction must be numbered as its log is.
            assert!(wal.append(10, next.xor(Some(SeqNo { term: 7, seq: 11 })), &[(0, Op::Insert(doc(5)))], false).is_err());
            wal.append(10, next, &[(0, Op::Insert(doc(5)))], false).unwrap();
            drop(wal);
            let (_, got) = Wal::open(&p, 11, numbered, false, false, &any).unwrap();
            assert_eq!((got.last().unwrap().txn, got.last().unwrap().seq), (10, next));
            // A log of the other kind is refused while it holds transactions.
            let e = Wal::open(&p, 11, !numbered, false, false, &any).err().unwrap().to_string();
            assert!(e.contains("sequence numbers, and the database"), "{e}");
            let (wal, got) = Wal::open(&p, 12, numbered, false, false, &any).unwrap();
            assert!(got.is_empty() && wal.is_empty());
        }
    }

    /// An empty log of the other kind starts again in the database's; a log
    /// naming a feature this version lacks is refused by name.
    #[test]
    fn a_log_says_what_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db-wal");
        let mut wal = Wal::create(&p, 11, false, false).unwrap();
        assert_eq!((wal.numbered(), wal.len()), (false, 16));
        wal.renumber(false).unwrap();
        assert_eq!((wal.numbered(), wal.len()), (true, 26));
        drop(wal);
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[..8], b"STRWAL02");
        assert_eq!(&bytes[16..], b"\x06\0\0\0seq_no");
        let (wal, _) = Wal::open(&p, 11, false, false, false, &any).unwrap();
        assert!(!wal.numbered() && wal.is_empty());
        drop(wal);
        assert_eq!(&std::fs::read(&p).unwrap()[..8], b"STRWAL01");
        let (wal, _) = Wal::open(&p, 11, true, false, false, &any).unwrap();
        assert!(wal.numbered() && wal.is_empty());
        drop(wal);
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(16);
        bytes.extend_from_slice(&13u32.to_le_bytes());
        bytes.extend_from_slice(b"seq_no,zstd:1");
        std::fs::write(&p, &bytes).unwrap();
        let e = Wal::open(&p, 11, true, false, false, &any).err().unwrap().to_string();
        assert!(e.contains(r#"uses feature "zstd:1""#), "{e}");
        assert!(read_sealed(&p, 11, &any).is_err());
    }

    /// An operation that does not fit (intact records the schema could
    /// not have committed) ends replay before its transaction, like a
    /// failed checksum.
    #[test]
    fn an_operation_that_does_not_fit_is_damage() {
        for (txns, numbered) in [(sample(), false), (numbered(), true)] {
            let (buf, ends) = encode(&txns);
            let no_set = |_: u32, op: &Op| !matches!(op, Op::Set { .. });
            assert_eq!(decode(&buf, numbered, &no_set), (decode(&buf[..ends[2]], numbered, &any).0, ends[2]));
            let no_table_5 = |t: u32, _: &Op| t != 5;
            assert_eq!(decode(&buf, numbered, &no_table_5).1, ends[3]);

            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("db-wal");
            let mut wal = Wal::create(&p, 11, numbered, false).unwrap();
            for (t, seq, ops) in &txns {
                wal.append(*t, *seq, ops, true).unwrap();
            }
            let header = wal.header;
            drop(wal);
            // With sync, three commits after the damage: refused unless asked.
            let err = Wal::open(&p, 11, numbered, true, false, &no_set).err().unwrap();
            assert!(err.to_string().contains("3 committed transactions follow the damage (up to txn 9)"), "{err}");
            let (wal, got) = Wal::open(&p, 11, numbered, true, true, &no_set).unwrap();
            assert_eq!(got.len(), 3);
            assert_eq!(wal.len(), header + ends[2] as u64);
            assert!(wal.dropped().is_some());
        }
    }
}