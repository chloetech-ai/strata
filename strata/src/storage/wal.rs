//! Write-ahead log: the `<path>-wal` sidecar.
//!
//! ```text
//! header:  "STRWAL01" | db_id: u64
//! record:  len: u32 | crc32(body): u32 | body = kind: u8, payload
//!          kind 1 insert: document     kind 2 delete: key value
//!          kind 3 commit: txn: u64, n_ops: u32
//! ```
//!
//! A transaction is its operation records followed by a commit record,
//! written with one `write` and made durable with one `fdatasync`. Replay
//! stops at the first record that is short or fails its checksum (a torn
//! tail) and drops operations not followed by a commit. The WAL is truncated
//! after each flush; transactions a crash left behind in it are skipped by
//! txn (see `Manifest::wal_txn`).
//!
//! Values are encoded as a tag byte and a little-endian payload: integers and
//! floats in 8 bytes, strings and vectors as a u32 length and the bytes (or
//! f32s).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use crate::error::{corrupt, Result};
use crate::value::{Document, Value};

const MAGIC: &[u8; 8] = b"STRWAL01";
const HEADER: u64 = 16;

const INSERT: u8 = 1;
const DELETE: u8 = 2;
const COMMIT: u8 = 3;

/// One logged operation.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Insert(Arc<Document>),
    Delete(Value),
}

/// A committed transaction read back from the log.
#[derive(Debug, PartialEq)]
pub struct WalTxn {
    pub txn: u64,
    pub ops: Vec<Op>,
}

/// The open log, appended to by the single writer.
pub struct Wal {
    file: File,
    db_id: u64,
    len: u64,
}

impl Wal {
    /// Creates (or truncates) the log.
    pub fn create(path: &Path, db_id: u64, sync: bool) -> Result<Wal> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        let mut wal = Wal { file, db_id, len: 0 };
        wal.reset(sync)?;
        Ok(wal)
    }

    /// Opens the log and returns its committed transactions. A missing log,
    /// or one left by another database (different id), counts as empty. A
    /// torn tail is cut off so that later appends follow the last commit.
    pub fn open(path: &Path, db_id: u64, sync: bool) -> Result<(Wal, Vec<WalTxn>)> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let size = file.metadata()?.len();
        let mut buf = vec![0u8; size as usize];
        file.read_exact_at(&mut buf, 0)?;
        let mut wal = Wal { file, db_id, len: size };
        if size < HEADER || &buf[..8] != MAGIC || buf[8..16] != db_id.to_le_bytes() {
            wal.reset(sync)?;
            return Ok((wal, Vec::new()));
        }
        let (txns, end) = decode(&buf[HEADER as usize..]);
        let end = HEADER + end as u64;
        if end < size {
            wal.file.set_len(end)?;
            if sync {
                wal.file.sync_data()?;
            }
            wal.len = end;
        }
        Ok((wal, txns))
    }

    /// Appends a committed transaction; durable on return when `sync`.
    pub fn append(&mut self, txn: u64, ops: &[Op], sync: bool) -> Result<()> {
        let buf = encode_txn(txn, ops);
        let res = self.file.write_all_at(&buf, self.len).and_then(|_| if sync { self.file.sync_data() } else { Ok(()) });
        if let Err(e) = res {
            // Cut off whatever part landed, so that later commits stay
            // reachable by replay; if even that fails the caller poisons
            // the database.
            self.file.set_len(self.len)?;
            return Err(e.into());
        }
        self.len += buf.len() as u64;
        Ok(())
    }

    /// Empties the log (after a flush made its contents durable elsewhere).
    pub fn reset(&mut self, sync: bool) -> Result<()> {
        self.file.set_len(0)?;
        let mut h = MAGIC.to_vec();
        h.extend_from_slice(&self.db_id.to_le_bytes());
        self.file.write_all_at(&h, 0)?;
        if sync {
            self.file.sync_data()?;
        }
        self.len = HEADER;
        Ok(())
    }

    /// Size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len <= HEADER
    }
}

/// The records of one transaction.
fn encode_txn(txn: u64, ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut body = Vec::new();
    for op in ops {
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
        }
        frame(&mut out, &body);
    }
    body.clear();
    body.push(COMMIT);
    body.extend_from_slice(&txn.to_le_bytes());
    body.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    frame(&mut out, &body);
    out
}

fn frame(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(body).to_le_bytes());
    out.extend_from_slice(body);
}

/// Committed transactions in `buf` (the log after its header) and the
/// length of the prefix they occupy.
fn decode(buf: &[u8]) -> (Vec<WalTxn>, usize) {
    let mut txns = Vec::new();
    let mut ops = Vec::new();
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
        match body[0] {
            INSERT => match get_doc(&mut r) {
                Ok(d) => ops.push(Op::Insert(Arc::new(d))),
                Err(_) => break,
            },
            DELETE => match get_value(&mut r) {
                Ok(v) => ops.push(Op::Delete(v)),
                Err(_) => break,
            },
            COMMIT => {
                let (Ok(txn), Ok(n)) = (r.u64(), r.u32()) else { break };
                if n as usize != ops.len() || txns.last().is_some_and(|t: &WalTxn| t.txn >= txn) {
                    break;
                }
                txns.push(WalTxn { txn, ops: std::mem::take(&mut ops) });
                end = pos;
            }
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

fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(NULL),
        Value::Int(x) => {
            out.push(INT);
            out.extend_from_slice(&x.to_le_bytes());
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

    fn doc(i: i64) -> Arc<Document> {
        Arc::new(vec![
            Value::Str(format!("key{i}")),
            Value::Int(i),
            Value::Float(i as f64 / 3.0),
            Value::Null,
            Value::Vector(vec![i as f32, -0.5, 1e-3].into()),
        ])
    }

    fn sample() -> Vec<(u64, Vec<Op>)> {
        vec![
            (3, vec![Op::Insert(doc(1)), Op::Insert(doc(2))]),
            (4, vec![Op::Delete(Value::Str("key1".into()))]),
            (5, vec![]),
            (7, vec![Op::Delete(Value::Int(9)), Op::Insert(doc(3))]),
        ]
    }

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        for (t, ops) in sample() {
            buf.extend(encode_txn(t, &ops));
        }
        let (txns, end) = decode(&buf);
        assert_eq!(end, buf.len());
        let want: Vec<WalTxn> = sample().into_iter().map(|(txn, ops)| WalTxn { txn, ops }).collect();
        assert_eq!(txns, want);
    }

    /// Cutting the log at every byte (a torn write) and appending garbage
    /// replays exactly the transactions whose records survived intact (a
    /// zero tail can restore zero bytes the cut removed).
    #[test]
    fn torn_tail_at_every_offset() {
        let mut buf = Vec::new();
        let mut ends = Vec::new();
        for (t, ops) in sample() {
            buf.extend(encode_txn(t, &ops));
            ends.push(buf.len());
        }
        for cut in 0..=buf.len() {
            for tail in [&[][..], &[0u8; 16][..], &[0xffu8; 7][..]] {
                let mut b = buf[..cut].to_vec();
                b.extend_from_slice(tail);
                let committed = ends.iter().filter(|&&e| b.starts_with(&buf[..e])).count();
                let (txns, end) = decode(&b);
                assert_eq!(txns.len(), committed, "cut at {cut}");
                assert_eq!(end, if committed == 0 { 0 } else { ends[committed - 1] });
            }
        }
        // A flipped bit inside the second transaction loses it and the rest.
        let mut b = buf.clone();
        b[ends[0] + 12] ^= 4;
        assert_eq!(decode(&b).0.len(), 1);
    }

    #[test]
    fn file_append_reopen_and_foreign_log() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db-wal");
        let mut wal = Wal::create(&p, 11, false).unwrap();
        for (t, ops) in sample() {
            wal.append(t, &ops, true).unwrap();
        }
        let len = wal.len();
        drop(wal);
        // Torn tail: half a record after the last commit is cut on open.
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        f.write_all_at(&encode_txn(9, &[Op::Insert(doc(4))])[..20], len).unwrap();
        let (mut wal, txns) = Wal::open(&p, 11, false).unwrap();
        assert_eq!(txns.len(), 4);
        assert_eq!(wal.len(), len);
        wal.append(10, &[Op::Insert(doc(5))], false).unwrap();
        drop(wal);
        let (_, txns) = Wal::open(&p, 11, false).unwrap();
        assert_eq!(txns.last().unwrap().txn, 10);
        let (wal, txns) = Wal::open(&p, 12, false).unwrap();
        assert!(txns.is_empty() && wal.is_empty());
    }
}
